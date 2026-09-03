//! Manage database users/roles inside a running instance via compute exec.
//!
//! Mirrors [`super::execute_query_usecase::ExecuteQueryUseCase`]: resolve the
//! provider + container from `.gfs` config, ask the provider to build the
//! in-instance role command, run it via [`Compute::exec`], and map the output.
//! No host-side DB client is used.

use std::path::Path;
use std::sync::Arc;

use thiserror::Error;

use crate::model::config::GfsConfig;
use crate::model::db_user::{
    DeployEnvSpec, GrantSpec, GrantableObject, ObjectPrivilege, PRESET_GROUP_ROLES, Privilege,
    RevokeSpec, RoleInfo, RolePreset, RoleSpec,
};
use crate::ports::compute::{Compute, ExecOutput, InstanceId};
use crate::ports::database_provider::{DatabaseProvider, DatabaseProviderRegistry};

#[derive(Debug, Error)]
pub enum ManageUsersError {
    #[error("config: {0}")]
    Config(String),

    #[error("not configured: {0}")]
    NotConfigured(String),

    #[error("provider not found: {0}")]
    ProviderNotFound(String),

    #[error("user management not supported by provider: {0}")]
    Unsupported(String),

    #[error("invalid input: {0}")]
    InvalidInput(String),

    #[error("compute: {0}")]
    Compute(String),

    #[error("operation failed (exit {exit_code}): {message}")]
    Failed { exit_code: i32, message: String },

    #[error("could not parse role list: {0}")]
    Parse(String),
}

pub struct ManageUsersUseCase<R: DatabaseProviderRegistry> {
    compute: Arc<dyn Compute>,
    registry: Arc<R>,
}

impl<R: DatabaseProviderRegistry> ManageUsersUseCase<R> {
    pub fn new(compute: Arc<dyn Compute>, registry: Arc<R>) -> Self {
        Self { compute, registry }
    }

    /// Resolve `(provider, container_name)` from the repo's `.gfs` config.
    fn resolve(
        &self,
        path: &Path,
    ) -> Result<(Arc<dyn DatabaseProvider>, String), ManageUsersError> {
        let config = GfsConfig::load(path).map_err(|e| ManageUsersError::Config(e.to_string()))?;

        let provider_name = config
            .environment
            .as_ref()
            .map(|e| e.database_provider.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                ManageUsersError::NotConfigured(
                    "no database provider configured (run gfs init)".into(),
                )
            })?
            .to_string();

        let provider = self
            .registry
            .get(&provider_name)
            .ok_or_else(|| ManageUsersError::ProviderNotFound(provider_name.clone()))?;

        // Resolved before the container, so an embedded provider gets an answer
        // rather than a closed loop: it has no runtime section, so requiring one
        // first advised `gfs compute start`, which then reported that there is
        // no container_name in the repo config. Neither command could succeed
        // and neither said why.
        if provider.local_engine().is_some() {
            return Err(ManageUsersError::Unsupported(format!(
                "'{provider_name}' is an embedded database — a file opened by this process, \
                 not a server. There is no connection to authenticate, so there are no roles, \
                 passwords or privileges to manage; access is whatever the filesystem grants \
                 to whoever can open the file"
            )));
        }

        let container_name = config
            .runtime
            .as_ref()
            .map(|r| r.container_name.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                ManageUsersError::NotConfigured(
                    "no container configured (run gfs compute start)".into(),
                )
            })?
            .to_string();

        Ok((provider, container_name))
    }

    /// Run an already-built in-instance command and return its output.
    async fn run(&self, container: &str, command: &str) -> Result<ExecOutput, ManageUsersError> {
        self.compute
            .exec(&InstanceId(container.to_string()), command, None)
            .await
            .map_err(|e| ManageUsersError::Compute(e.to_string()))
    }

    /// Create a login role (optionally with a preset).
    pub async fn create_role(&self, path: &Path, spec: &RoleSpec) -> Result<(), ManageUsersError> {
        require_password(&spec.password)?;
        reject_reserved_role(&spec.username)?;
        let (provider, container) = self.resolve(path)?;
        let command = provider
            .create_role_command(spec)
            .map_err(map_provider_err)?;
        expect_success(self.run(&container, &command).await?)
    }

    /// Set / rotate a role's password.
    pub async fn set_password(
        &self,
        path: &Path,
        username: &str,
        password: &str,
    ) -> Result<(), ManageUsersError> {
        require_password(password)?;
        reject_reserved_role(username)?;
        let (provider, container) = self.resolve(path)?;
        let command = provider
            .alter_password_command(username, password)
            .map_err(map_provider_err)?;
        expect_success(self.run(&container, &command).await?)?;
        // Immediate revocation: kill any session still authenticated with the OLD
        // password so a rotation takes effect at once, not only when that session
        // disconnects. Best-effort — a terminate failure must not fail the rotation,
        // which has already changed the credential. (On the checkout re-key path the
        // pod is fresh, so this is a no-op.)
        if let Err(e) = self.terminate_user_sessions(path, username).await {
            tracing::warn!(
                user = %username,
                error = %e,
                "set-password: could not terminate live sessions after rotation"
            );
        }
        Ok(())
    }

    /// Reset the deploy `owner`'s password as a **platform** operation (clone
    /// fresh-reset): a clone inherits the parent's `owner` from the seeded
    /// snapshot, so the platform rotates it to a fresh, child-owned credential.
    /// Unlike [`Self::set_password`], this permits the reserved `owner` role (the
    /// caller is the platform, not a customer) but still refuses the management
    /// superuser. Not exposed on any customer-facing route.
    pub async fn reset_deploy_owner_password(
        &self,
        path: &Path,
        owner: &str,
        password: &str,
    ) -> Result<(), ManageUsersError> {
        require_password(password)?;
        reject_superuser_role(owner)?;
        let (provider, container) = self.resolve(path)?;
        let command = provider
            .alter_password_command(owner, password)
            .map_err(map_provider_err)?;
        expect_success(self.run(&container, &command).await?)
    }

    /// Drop a role.
    /// Disable `username`'s ability to open NEW sessions (`ALTER ROLE … NOLOGIN`),
    /// committed immediately. Used before a drop to close the reconnect window while
    /// the role's live backends are terminated. Refuses reserved roles.
    ///
    /// # Errors
    /// Propagates a provider or exec failure.
    pub async fn disable_login(&self, path: &Path, username: &str) -> Result<(), ManageUsersError> {
        reject_reserved_role(username)?;
        let (provider, container) = self.resolve(path)?;
        let command = provider
            .disable_login_command(username)
            .map_err(map_provider_err)?;
        expect_success(self.run(&container, &command).await?)
    }

    pub async fn drop_role(&self, path: &Path, username: &str) -> Result<(), ManageUsersError> {
        reject_reserved_role(username)?;
        // Immediate revocation with no reconnect window: first disable login
        // (committed, so no NEW session can authenticate), then terminate the live
        // backends, then drop. Without the disable-login step a client with the
        // still-valid credential could reconnect during the terminate→drop gap and
        // survive the drop as an orphan. Both pre-steps are best-effort — a failure
        // must not block the removal, which is the actual guarantee.
        if let Err(e) = self.disable_login(path, username).await {
            tracing::warn!(
                user = %username,
                error = %e,
                "drop: could not disable login before terminate; a brief reconnect window remains"
            );
        }
        if let Err(e) = self.terminate_user_sessions(path, username).await {
            tracing::warn!(
                user = %username,
                error = %e,
                "drop: could not terminate live sessions before drop; proceeding with the drop"
            );
        }
        let (provider, container) = self.resolve(path)?;
        // Reassign the role's objects to the deploy `owner` (the customer's own
        // role) rather than the management superuser running this exec, so a
        // dropped user's tables don't silently move to an unusable platform role.
        // Falls back to CURRENT_USER when the owner can't be resolved.
        let reassign_owner = self.detect_deploy_owner(path).await;
        let command = provider
            .drop_role_command(username, reassign_owner.as_deref())
            .map_err(map_provider_err)?;
        expect_success(self.run(&container, &command).await?)
    }

    /// Neutralize a role we intend to remove but cannot [`Self::drop_role`] (it
    /// owns objects in the restored older data version): disable login and
    /// overwrite its password with a fresh unknowable one. Degrades access to
    /// *disabled* without mutating customer data — the fail-closed guarantee that
    /// does not depend on DROP. The caller supplies `new_password`; it is meant to
    /// be thrown away (never surfaced, never vaulted — its unknowability is the
    /// point), so callers pass a fresh random value.
    pub async fn quarantine_role(
        &self,
        path: &Path,
        username: &str,
        new_password: &str,
    ) -> Result<(), ManageUsersError> {
        reject_reserved_role(username)?;
        let (provider, container) = self.resolve(path)?;
        let command = provider
            .quarantine_role_command(username, new_password)
            .map_err(map_provider_err)?;
        expect_success(self.run(&container, &command).await?)
    }

    /// Apply a role preset to an existing role.
    ///
    /// `default_privileges_owner`, when set, is the role whose future objects the
    /// preset's default privileges should cover (the customer's `owner` role in a
    /// deploy). `None` role-scopes the defaults to the connecting role (single-node).
    pub async fn apply_preset(
        &self,
        path: &Path,
        username: &str,
        preset: RolePreset,
        default_privileges_owner: Option<&str>,
    ) -> Result<(), ManageUsersError> {
        reject_reserved_role(username)?;
        let (provider, container) = self.resolve(path)?;
        let command = provider
            .apply_preset_command(username, preset, default_privileges_owner)
            .map_err(map_provider_err)?;
        expect_success(self.run(&container, &command).await?)
    }

    /// Bootstrap a database's deploy environment: create the `NOLOGIN`
    /// group + the least-privileged `owner` login + grants + role-scoped default
    /// privileges, in one transaction, as the management superuser via the exec
    /// seam. Idempotency is the caller's concern (run on fresh deploy only).
    pub async fn provision_deploy_env(
        &self,
        path: &Path,
        spec: &DeployEnvSpec,
    ) -> Result<(), ManageUsersError> {
        require_password(&spec.owner_password)?;
        let (provider, container) = self.resolve(path)?;
        let command = provider
            .bootstrap_deploy_env_command(spec)
            .map_err(map_provider_err)?;
        expect_success(self.run(&container, &command).await?)
    }

    /// List login roles (never a password).
    pub async fn list_roles(&self, path: &Path) -> Result<Vec<RoleInfo>, ManageUsersError> {
        let (provider, container) = self.resolve(path)?;
        let command = provider.list_roles_command().map_err(map_provider_err)?;
        let output = self.run(&container, &command).await?;
        if output.exit_code != 0 {
            return Err(fail(output));
        }
        serde_json::from_str(output.stdout.trim())
            .map_err(|e| ManageUsersError::Parse(e.to_string()))
    }

    /// Read `username`'s stored password verifier (`pg_authid.rolpassword`), or
    /// `None` when the role is absent or carries no password. The verifier is a
    /// one-way value: it is what the durability store keeps at rest instead of the
    /// plaintext, and re-key compares it by value to detect credential drift.
    ///
    /// # Errors
    /// Propagates a provider or exec failure.
    pub async fn user_verifier(
        &self,
        path: &Path,
        username: &str,
    ) -> Result<Option<String>, ManageUsersError> {
        let (provider, container) = self.resolve(path)?;
        let command = provider
            .user_verifier_command(username)
            .map_err(map_provider_err)?;
        let output = self.run(&container, &command).await?;
        if output.exit_code != 0 {
            return Err(fail(output));
        }
        let verifier = output.stdout.trim();
        Ok((!verifier.is_empty()).then(|| verifier.to_string()))
    }

    /// Terminate every live backend for `username` except the management session,
    /// returning the number terminated. Makes a drop or password rotation take
    /// effect on open connections immediately. Refuses reserved roles — the
    /// platform never kills its own management / bootstrap sessions.
    ///
    /// # Errors
    /// Propagates a provider or exec failure; a parse error on a non-numeric count.
    pub async fn terminate_user_sessions(
        &self,
        path: &Path,
        username: &str,
    ) -> Result<u64, ManageUsersError> {
        reject_reserved_role(username)?;
        let (provider, container) = self.resolve(path)?;
        let command = provider
            .terminate_user_sessions_command(username)
            .map_err(map_provider_err)?;
        let output = self.run(&container, &command).await?;
        if output.exit_code != 0 {
            return Err(fail(output));
        }
        output
            .stdout
            .trim()
            .parse::<u64>()
            .map_err(|e| ManageUsersError::Parse(e.to_string()))
    }

    /// Detect the deploy's object-creating `owner` role so a
    /// preset's `ALTER DEFAULT PRIVILEGES` covers the customer's future tables,
    /// not the connecting admin's. The CLI/MCP don't know the deploy owner: if the
    /// conventional `owner` role exists, scope preset defaults to it; otherwise
    /// `None` (single-node — defaults stay connecting-role-scoped).
    pub async fn detect_deploy_owner(&self, path: &Path) -> Option<String> {
        self.detect_deploy_owner_checked(path).await.ok().flatten()
    }

    /// Like [`Self::detect_deploy_owner`] but distinguishes a detection *failure*
    /// (listing roles errored) from a database that genuinely has no `owner` role.
    /// The clone fresh-reset needs this: a detection error must fail **closed** —
    /// never silently skip rotating the inherited `owner` password,
    /// which would leave the parent's credential live on the child. A legitimate
    /// absence (`Ok(None)`, a legacy parent) safely takes the
    /// parent-credential path.
    ///
    /// # Errors
    /// Propagates a `list_roles` failure (exec/parse/non-zero exit).
    pub async fn detect_deploy_owner_checked(
        &self,
        path: &Path,
    ) -> Result<Option<String>, ManageUsersError> {
        const DEPLOY_OWNER_ROLE: &str = "owner";
        let roles = self.list_roles(path).await?;
        Ok(roles
            .iter()
            .any(|r| r.username == DEPLOY_OWNER_ROLE)
            .then(|| DEPLOY_OWNER_ROLE.to_string()))
    }

    /// Grant object-level privileges on `spec.object` to `spec.role`.
    pub async fn grant(&self, path: &Path, spec: &GrantSpec) -> Result<(), ManageUsersError> {
        reject_reserved_role(&spec.role)?;
        validate_privileges(&spec.privileges, &spec.object)?;
        let (provider, container) = self.resolve(path)?;
        let command = provider.grant_command(spec).map_err(map_provider_err)?;
        expect_success(self.run(&container, &command).await?)
    }

    /// Revoke object-level privileges on `spec.object` from `spec.role`.
    pub async fn revoke(&self, path: &Path, spec: &RevokeSpec) -> Result<(), ManageUsersError> {
        reject_reserved_role(&spec.role)?;
        validate_privileges(&spec.privileges, &spec.object)?;
        let (provider, container) = self.resolve(path)?;
        let command = provider.revoke_command(spec).map_err(map_provider_err)?;
        expect_success(self.run(&container, &command).await?)
    }

    /// List a role's effective object privileges (never a secret).
    pub async fn list_privileges(
        &self,
        path: &Path,
        role: &str,
    ) -> Result<Vec<ObjectPrivilege>, ManageUsersError> {
        let (provider, container) = self.resolve(path)?;
        let command = provider
            .list_privileges_command(role)
            .map_err(map_provider_err)?;
        let output = self.run(&container, &command).await?;
        if output.exit_code != 0 {
            return Err(fail(output));
        }
        serde_json::from_str(output.stdout.trim())
            .map_err(|e| ManageUsersError::Parse(e.to_string()))
    }
}

/// Reject a privilege set that is empty or contains a privilege not valid for the
/// target object type (the domain allow-list), *before* the command is built —
/// engine-independent defence-in-depth (the provider re-checks too).
fn validate_privileges(
    privileges: &[Privilege],
    object: &GrantableObject,
) -> Result<(), ManageUsersError> {
    if privileges.is_empty() {
        return Err(ManageUsersError::InvalidInput(
            "at least one privilege is required".into(),
        ));
    }
    for p in privileges {
        if !p.is_valid_for(object) {
            return Err(ManageUsersError::InvalidInput(format!(
                "privilege '{}' is not valid for the target object type",
                p.as_str()
            )));
        }
    }
    Ok(())
}

/// A mutating op succeeded iff the exit code is zero.
fn expect_success(output: ExecOutput) -> Result<(), ManageUsersError> {
    if output.exit_code == 0 {
        Ok(())
    } else {
        Err(fail(output))
    }
}

fn fail(output: ExecOutput) -> ManageUsersError {
    let message = if output.stderr.trim().is_empty() {
        output.stdout.trim().to_string()
    } else {
        output.stderr.trim().to_string()
    };
    ManageUsersError::Failed {
        exit_code: output.exit_code,
        message,
    }
}

/// Reject an empty password — a login role with no password is a footgun.
fn require_password(password: &str) -> Result<(), ManageUsersError> {
    if password.is_empty() {
        Err(ManageUsersError::InvalidInput(
            "password must not be empty".into(),
        ))
    } else {
        Ok(())
    }
}

/// The platform's load-bearing roles, never client roles: the engine
/// connection superuser `postgres`, the bootstrap super `gfs_super`, the
/// customer's least-privileged login `owner`, and the `developers` group. Client
/// user management (`gfs user`) refuses to mutate any of them — dropping the
/// connection superuser makes `DROP OWNED BY` wedge the session; dropping `owner`
/// destroys the customer's primary login; rotating a password or privileges
/// out-of-band desyncs the deploy's stored credential/connection string. Fail
/// fast with a clear message instead.
const RESERVED_ROLES: [&str; 4] = ["gfs_super", "postgres", "owner", "developers"];

/// The management superusers, whose passwords the platform never rotates through
/// any user-facing or fresh-reset path — they live in `credentials.toml`, not the
/// revealable vault.
const SUPERUSER_ROLES: [&str; 2] = ["gfs_super", "postgres"];

/// Refuse the management superuser (a subset of [`reject_reserved_role`]) while
/// permitting the platform-provisioned `owner`/`developers`. Used by the
/// platform-only owner-password reset (clone fresh-reset).
fn reject_superuser_role(username: &str) -> Result<(), ManageUsersError> {
    if SUPERUSER_ROLES.contains(&username.to_ascii_lowercase().as_str()) {
        Err(ManageUsersError::InvalidInput(format!(
            "'{username}' is a management superuser and cannot be rotated"
        )))
    } else {
        Ok(())
    }
}

/// Whether `username` is a reserved platform role (`owner`/`developers` + the
/// management superusers + the preset group roles). Public so the reconcile
/// re-key can skip them — a reserved role must not be re-keyed as a normal
/// managed user. The preset group roles (`gfs_readonly`/`gfs_readwrite`/
/// `gfs_admin`) carry the platform-managed privilege level; a client must never
/// create, drop, or rotate one. Case-insensitive: a look-alike like `POSTGRES`
/// is a *distinct* Postgres role, but treating every case variant as reserved
/// avoids confusion with the real one.
pub fn is_reserved_role(username: &str) -> bool {
    let lower = username.to_ascii_lowercase();
    RESERVED_ROLES.contains(&lower.as_str()) || PRESET_GROUP_ROLES.contains(&lower.as_str())
}

fn reject_reserved_role(username: &str) -> Result<(), ManageUsersError> {
    if is_reserved_role(username) {
        Err(ManageUsersError::InvalidInput(format!(
            "'{username}' is a reserved platform role and cannot be modified via user management"
        )))
    } else {
        Ok(())
    }
}

/// Map a provider error to the right domain error: validation failures (bad
/// identifier, delimiter collision) are `InvalidInput`, not `Unsupported`.
fn map_provider_err(e: crate::ports::database_provider::ProviderError) -> ManageUsersError {
    use crate::ports::database_provider::ProviderError;
    match e {
        ProviderError::InvalidParams(m) => ManageUsersError::InvalidInput(m),
        other => ManageUsersError::Unsupported(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Mutex;

    use async_trait::async_trait;

    #[test]
    fn reserved_roles_cannot_be_dropped() {
        use super::reject_reserved_role;
        assert!(reject_reserved_role("postgres").is_err());
        assert!(reject_reserved_role("gfs_super").is_err());
        // The customer's load-bearing deploy roles are protected too (F-04).
        assert!(reject_reserved_role("owner").is_err());
        assert!(reject_reserved_role("developers").is_err());
        // Case variants of a reserved name are refused (no confusing look-alikes).
        assert!(reject_reserved_role("POSTGRES").is_err());
        assert!(reject_reserved_role("Owner").is_err());
        // The preset group roles carry the platform-managed level — a client must
        // never create, drop, or rotate one (any case variant).
        assert!(reject_reserved_role("gfs_readonly").is_err());
        assert!(reject_reserved_role("gfs_readwrite").is_err());
        assert!(reject_reserved_role("gfs_admin").is_err());
        assert!(reject_reserved_role("GFS_Admin").is_err());
        assert!(reject_reserved_role("app_rw").is_ok());
    }

    #[test]
    fn superuser_guard_permits_owner_but_refuses_the_management_super() {
        use super::reject_superuser_role;
        // The platform owner-reset (clone fresh-reset) rotates `owner` — allowed.
        assert!(reject_superuser_role("owner").is_ok());
        assert!(reject_superuser_role("developers").is_ok());
        // ...but never the management superuser, in any case.
        assert!(reject_superuser_role("gfs_super").is_err());
        assert!(reject_superuser_role("postgres").is_err());
        assert!(reject_superuser_role("POSTGRES").is_err());
    }
    use tempfile::TempDir;

    use super::*;
    use crate::model::config::{EnvironmentConfig, RuntimeConfig};
    use crate::ports::compute::{
        ComputeCapabilities, ComputeDefinition, InstanceConnectionInfo, InstanceState,
        InstanceStatus, LogEntry, LogsOptions, PortMapping, StartOptions,
    };
    use crate::ports::database_provider::{
        ConnectionParams, ContainerProvider, DatabaseProvider, DatabaseProviderArg,
        InMemoryDatabaseProviderRegistry, ProviderError, Result as RegistryResult, SIGTERM,
        SupportedFeature,
    };

    /// Compute mock: records the last `exec` command and returns a canned output.
    #[derive(Default)]
    struct MockCompute {
        last_command: Mutex<Option<String>>,
        stdout: String,
        stderr: String,
        exit_code: i32,
    }

    type CResult<T> = crate::ports::compute::Result<T>;

    #[async_trait]
    impl Compute for MockCompute {
        async fn provision(&self, _: &ComputeDefinition) -> CResult<InstanceId> {
            Ok(InstanceId("mock".into()))
        }
        async fn start(&self, id: &InstanceId, _: StartOptions) -> CResult<InstanceStatus> {
            Ok(running(id))
        }
        async fn stop(&self, id: &InstanceId) -> CResult<InstanceStatus> {
            Ok(running(id))
        }
        async fn restart(&self, id: &InstanceId) -> CResult<InstanceStatus> {
            Ok(running(id))
        }
        async fn status(&self, id: &InstanceId) -> CResult<InstanceStatus> {
            Ok(running(id))
        }
        async fn prepare_for_snapshot(&self, _: &InstanceId, _: &[String]) -> CResult<()> {
            Ok(())
        }
        async fn logs(&self, _: &InstanceId, _: LogsOptions) -> CResult<Vec<LogEntry>> {
            Ok(vec![])
        }
        async fn pause(&self, id: &InstanceId) -> CResult<InstanceStatus> {
            Ok(running(id))
        }
        async fn unpause(&self, id: &InstanceId) -> CResult<InstanceStatus> {
            Ok(running(id))
        }
        async fn get_connection_info(
            &self,
            _: &InstanceId,
            port: u16,
        ) -> CResult<InstanceConnectionInfo> {
            Ok(InstanceConnectionInfo {
                host: "127.0.0.1".into(),
                port,
                env: vec![],
            })
        }
        async fn get_instance_data_mount_host_path(
            &self,
            _: &InstanceId,
            _: &str,
        ) -> CResult<Option<PathBuf>> {
            Ok(None)
        }
        async fn remove_instance(&self, _: &InstanceId) -> CResult<()> {
            Ok(())
        }
        async fn get_task_connection_info(
            &self,
            _: &InstanceId,
            port: u16,
        ) -> CResult<InstanceConnectionInfo> {
            Ok(InstanceConnectionInfo {
                host: "127.0.0.1".into(),
                port,
                env: vec![],
            })
        }
        async fn run_task(
            &self,
            _: &ComputeDefinition,
            _: &str,
            _: Option<&InstanceId>,
        ) -> CResult<ExecOutput> {
            Ok(ok_output())
        }
        async fn capabilities(&self) -> CResult<ComputeCapabilities> {
            Ok(ComputeCapabilities {
                supports_stream_snapshot: false,
                supports_exec_as_root: true,
                db_live_during_snapshot: false,
            })
        }
        async fn exec(
            &self,
            _: &InstanceId,
            command: &str,
            _: Option<&str>,
        ) -> CResult<ExecOutput> {
            *self.last_command.lock().unwrap() = Some(command.to_string());
            Ok(ExecOutput {
                exit_code: self.exit_code,
                stdout: self.stdout.clone(),
                stderr: self.stderr.clone(),
            })
        }
    }

    fn running(id: &InstanceId) -> InstanceStatus {
        InstanceStatus {
            id: id.clone(),
            state: InstanceState::Running,
            pid: None,
            started_at: None,
            exit_code: None,
        }
    }

    fn ok_output() -> ExecOutput {
        ExecOutput {
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
        }
    }

    /// Provider mock: role methods return marker commands; `support_users=false`
    /// makes them report the feature as unsupported.
    struct MockRoleProvider {
        support_users: bool,
    }

    impl MockRoleProvider {
        fn guard(&self) -> std::result::Result<(), ProviderError> {
            if self.support_users {
                Ok(())
            } else {
                Err(ProviderError::UnsupportedFormat("users".into()))
            }
        }
    }

    impl DatabaseProvider for MockRoleProvider {
        fn name(&self) -> &str {
            "mock-role"
        }
        fn connection_string(
            &self,
            _: &ConnectionParams,
        ) -> std::result::Result<String, ProviderError> {
            Ok("mock://localhost".into())
        }
        fn supported_versions(&self) -> Vec<String> {
            vec!["latest".into()]
        }
        fn supported_features(&self) -> Vec<SupportedFeature> {
            vec![]
        }
        fn query_client_command(
            &self,
            _: &ConnectionParams,
            _: Option<&str>,
        ) -> std::result::Result<std::process::Command, ProviderError> {
            Ok(std::process::Command::new("true"))
        }
        fn create_role_command(
            &self,
            spec: &RoleSpec,
        ) -> std::result::Result<String, ProviderError> {
            self.guard()?;
            Ok(format!("MOCK-CREATE:{}", spec.username))
        }
        fn alter_password_command(
            &self,
            username: &str,
            _: &str,
        ) -> std::result::Result<String, ProviderError> {
            self.guard()?;
            Ok(format!("MOCK-ALTER:{username}"))
        }
        fn drop_role_command(
            &self,
            username: &str,
            reassign_owned_to: Option<&str>,
        ) -> std::result::Result<String, ProviderError> {
            self.guard()?;
            Ok(format!(
                "MOCK-DROP:{username}:reassign={}",
                reassign_owned_to.unwrap_or("CURRENT_USER")
            ))
        }
        fn list_roles_command(&self) -> std::result::Result<String, ProviderError> {
            self.guard()?;
            Ok("MOCK-LIST".into())
        }
        fn apply_preset_command(
            &self,
            username: &str,
            _: RolePreset,
            _: Option<&str>,
        ) -> std::result::Result<String, ProviderError> {
            self.guard()?;
            Ok(format!("MOCK-PRESET:{username}"))
        }

        fn bootstrap_deploy_env_command(
            &self,
            spec: &DeployEnvSpec,
        ) -> std::result::Result<String, ProviderError> {
            self.guard()?;
            Ok(format!("MOCK-DEPLOYENV:{}:{}", spec.owner, spec.group))
        }

        fn grant_command(&self, spec: &GrantSpec) -> std::result::Result<String, ProviderError> {
            self.guard()?;
            Ok(format!("MOCK-GRANT:{}", spec.role))
        }

        fn revoke_command(&self, spec: &RevokeSpec) -> std::result::Result<String, ProviderError> {
            self.guard()?;
            Ok(format!("MOCK-REVOKE:{}", spec.role))
        }

        fn list_privileges_command(
            &self,
            role: &str,
        ) -> std::result::Result<String, ProviderError> {
            self.guard()?;
            Ok(format!("MOCK-LISTPRIVS:{role}"))
        }

        fn container(&self) -> Option<&dyn ContainerProvider> {
            Some(self)
        }
    }

    impl ContainerProvider for MockRoleProvider {
        fn definition(&self) -> ComputeDefinition {
            ComputeDefinition {
                labels: Default::default(),
                image: "mock:latest".into(),
                env: vec![],
                ports: vec![PortMapping {
                    compute_port: 5432,
                    host_port: None,
                }],
                data_dir: PathBuf::from("/data"),
                host_data_dir: None,
                user: None,
                logs_dir: None,
                conf_dir: None,
                args: vec![],
            }
        }
        fn default_port(&self) -> u16 {
            5432
        }
        fn default_args(&self) -> Vec<DatabaseProviderArg> {
            vec![]
        }
        fn default_signal(&self) -> u32 {
            SIGTERM
        }
        fn prepare_for_snapshot(&self, _: &ConnectionParams) -> RegistryResult<Vec<String>> {
            Ok(vec![])
        }
    }

    fn repo_with_config(container: &str) -> (TempDir, PathBuf) {
        let temp = TempDir::new().expect("tempdir");
        let path = temp.path().to_path_buf();
        std::fs::create_dir_all(path.join(".gfs")).expect("create .gfs");
        let config = GfsConfig {
            mount_point: None,
            version: String::new(),
            description: String::new(),
            user: None,
            environment: Some(EnvironmentConfig {
                database_provider: "mock-role".into(),
                database_version: "17".into(),
                database_port: None,
                display_name: None,
            }),
            runtime: Some(RuntimeConfig {
                runtime_provider: "docker".into(),
                runtime_version: "latest".into(),
                container_name: container.into(),
            }),
            storage: None,
            compute: None,
        };
        config.save(&path).expect("save config");
        (temp, path)
    }

    fn use_case(
        compute: MockCompute,
        support_users: bool,
    ) -> (
        ManageUsersUseCase<InMemoryDatabaseProviderRegistry>,
        Arc<MockCompute>,
    ) {
        let compute = Arc::new(compute);
        let registry = InMemoryDatabaseProviderRegistry::new();
        registry
            .register(Arc::new(MockRoleProvider { support_users }))
            .unwrap();
        (
            ManageUsersUseCase::new(compute.clone(), Arc::new(registry)),
            compute,
        )
    }

    #[tokio::test]
    async fn create_role_execs_the_provider_command() {
        let (_temp, repo) = repo_with_config("pg-c1");
        let (uc, compute) = use_case(MockCompute::default(), true);
        uc.create_role(
            &repo,
            &RoleSpec {
                username: "alice".into(),
                password: "pw".into(),
                preset: None,
                default_privileges_owner: None,
            },
        )
        .await
        .expect("ok");
        assert_eq!(
            compute.last_command.lock().unwrap().clone(),
            Some("MOCK-CREATE:alice".into())
        );
    }

    fn deploy_env_spec(owner_password: &str) -> DeployEnvSpec {
        DeployEnvSpec {
            owner: "app_owner".into(),
            owner_password: owner_password.into(),
            group: "developers".into(),
            database: "appdb".into(),
        }
    }

    #[tokio::test]
    async fn provision_deploy_env_execs_the_provider_command() {
        let (_temp, repo) = repo_with_config("pg-c1");
        let (uc, compute) = use_case(MockCompute::default(), true);
        uc.provision_deploy_env(&repo, &deploy_env_spec("pw"))
            .await
            .expect("ok");
        assert_eq!(
            compute.last_command.lock().unwrap().clone(),
            Some("MOCK-DEPLOYENV:app_owner:developers".into())
        );
    }

    #[tokio::test]
    async fn provision_deploy_env_rejects_empty_owner_password() {
        let (_temp, repo) = repo_with_config("pg-c1");
        let (uc, _compute) = use_case(MockCompute::default(), true);
        let err = uc
            .provision_deploy_env(&repo, &deploy_env_spec(""))
            .await
            .expect_err("empty password must be rejected");
        assert!(matches!(err, ManageUsersError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn list_roles_parses_json() {
        let (_temp, repo) = repo_with_config("pg-c1");
        let compute = MockCompute {
            stdout: r#"[{"username":"alice","can_login":true,"is_superuser":false}]"#.into(),
            ..Default::default()
        };
        let (uc, _c) = use_case(compute, true);
        let roles = uc.list_roles(&repo).await.expect("ok");
        assert_eq!(roles.len(), 1);
        assert_eq!(roles[0].username, "alice");
        assert!(roles[0].can_login && !roles[0].is_superuser);
    }

    #[tokio::test]
    async fn detect_deploy_owner_checked_distinguishes_absence_from_failure() {
        // Present -> Ok(Some("owner")).
        let (_t1, repo1) = repo_with_config("pg-c1");
        let uc1 = use_case(
            MockCompute {
                stdout: r#"[{"username":"owner","can_login":true,"is_superuser":false}]"#.into(),
                ..Default::default()
            },
            true,
        )
        .0;
        assert_eq!(
            uc1.detect_deploy_owner_checked(&repo1).await.unwrap(),
            Some("owner".to_string())
        );

        // Genuinely absent -> Ok(None): a legacy parent, safe to skip the reset.
        let (_t2, repo2) = repo_with_config("pg-c1");
        let uc2 = use_case(
            MockCompute {
                stdout: r#"[{"username":"alice","can_login":true,"is_superuser":false}]"#.into(),
                ..Default::default()
            },
            true,
        )
        .0;
        assert_eq!(uc2.detect_deploy_owner_checked(&repo2).await.unwrap(), None);

        // Detection FAILURE -> Err (F1): a listing error must propagate, never be
        // swallowed to Ok(None) — that would make the clone fresh-reset silently
        // skip and leave the parent's owner password live on the child.
        let (_t3, repo3) = repo_with_config("pg-c1");
        let uc3 = use_case(
            MockCompute {
                exit_code: 1,
                stderr: "list roles boom".into(),
                ..Default::default()
            },
            true,
        )
        .0;
        assert!(
            uc3.detect_deploy_owner_checked(&repo3).await.is_err(),
            "a listing error must propagate, not become Ok(None)"
        );
        // The lenient variant still degrades to None for its preset-scoping callers.
        assert_eq!(uc3.detect_deploy_owner(&repo3).await, None);
    }

    #[tokio::test]
    async fn non_zero_exit_is_failed_with_message() {
        let (_temp, repo) = repo_with_config("pg-c1");
        let compute = MockCompute {
            exit_code: 1,
            stderr: "role \"alice\" already exists".into(),
            ..Default::default()
        };
        let (uc, _c) = use_case(compute, true);
        let err = uc
            .create_role(
                &repo,
                &RoleSpec {
                    username: "alice".into(),
                    password: "pw".into(),
                    preset: None,
                    default_privileges_owner: None,
                },
            )
            .await
            .unwrap_err();
        match err {
            ManageUsersError::Failed { exit_code, message } => {
                assert_eq!(exit_code, 1);
                assert!(message.contains("already exists"));
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unsupported_provider_maps_to_unsupported() {
        let (_temp, repo) = repo_with_config("pg-c1");
        let (uc, _c) = use_case(MockCompute::default(), /* support_users */ false);
        let err = uc.drop_role(&repo, "alice").await.unwrap_err();
        assert!(matches!(err, ManageUsersError::Unsupported(_)));
    }

    #[tokio::test]
    async fn empty_password_is_rejected() {
        let (_temp, repo) = repo_with_config("pg-c1");
        let (uc, _c) = use_case(MockCompute::default(), true);
        let err = uc
            .create_role(
                &repo,
                &RoleSpec {
                    username: "alice".into(),
                    password: String::new(),
                    preset: None,
                    default_privileges_owner: None,
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ManageUsersError::InvalidInput(_)));
    }

    // ----- object-level grant / revoke / list-privileges (phase 2) -----

    fn grant_spec(role: &str, object: GrantableObject, privileges: Vec<Privilege>) -> GrantSpec {
        GrantSpec {
            role: role.into(),
            object,
            privileges,
            with_grant_option: false,
            apply_to_future: None,
        }
    }

    #[tokio::test]
    async fn grant_execs_the_provider_command() {
        let (_temp, repo) = repo_with_config("pg-c1");
        let (uc, compute) = use_case(MockCompute::default(), true);
        uc.grant(
            &repo,
            &grant_spec(
                "app_ro",
                GrantableObject::Table {
                    schema: "public".into(),
                    name: "t".into(),
                },
                vec![Privilege::Select],
            ),
        )
        .await
        .expect("ok");
        assert_eq!(
            compute.last_command.lock().unwrap().clone(),
            Some("MOCK-GRANT:app_ro".into())
        );
    }

    #[tokio::test]
    async fn revoke_execs_the_provider_command() {
        let (_temp, repo) = repo_with_config("pg-c1");
        let (uc, compute) = use_case(MockCompute::default(), true);
        uc.revoke(
            &repo,
            &RevokeSpec {
                role: "app_rw".into(),
                object: GrantableObject::Table {
                    schema: "public".into(),
                    name: "t".into(),
                },
                privileges: vec![Privilege::Insert],
                cascade: false,
            },
        )
        .await
        .expect("ok");
        assert_eq!(
            compute.last_command.lock().unwrap().clone(),
            Some("MOCK-REVOKE:app_rw".into())
        );
    }

    #[tokio::test]
    async fn invalid_privilege_is_rejected_before_any_exec() {
        let (_temp, repo) = repo_with_config("pg-c1");
        let (uc, compute) = use_case(MockCompute::default(), true);
        // INSERT is not valid on a sequence.
        let err = uc
            .grant(
                &repo,
                &grant_spec(
                    "app_ro",
                    GrantableObject::Sequence {
                        schema: "public".into(),
                        name: "q".into(),
                    },
                    vec![Privilege::Insert],
                ),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ManageUsersError::InvalidInput(_)));
        // The domain guard fired before resolve/exec — nothing reached compute.
        assert!(
            compute.last_command.lock().unwrap().is_none(),
            "no command must be exec'd for an invalid privilege"
        );
    }

    #[tokio::test]
    async fn empty_privileges_is_rejected() {
        let (_temp, repo) = repo_with_config("pg-c1");
        let (uc, _c) = use_case(MockCompute::default(), true);
        let err = uc
            .grant(
                &repo,
                &grant_spec(
                    "app_ro",
                    GrantableObject::Table {
                        schema: "public".into(),
                        name: "t".into(),
                    },
                    vec![],
                ),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ManageUsersError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn list_privileges_parses_json() {
        let (_temp, repo) = repo_with_config("pg-c1");
        let compute = MockCompute {
            stdout: r#"[{"object_type":"table","object_name":"public.t","privilege":"select","grantable":false}]"#.into(),
            ..Default::default()
        };
        let (uc, _c) = use_case(compute, true);
        let privs = uc.list_privileges(&repo, "app_ro").await.expect("ok");
        assert_eq!(privs.len(), 1);
        assert_eq!(privs[0].object_type, "table");
        assert_eq!(privs[0].object_name, "public.t");
        assert_eq!(privs[0].privilege, "select");
        assert!(!privs[0].grantable);
    }

    #[tokio::test]
    async fn grant_on_unsupported_provider_maps_to_unsupported() {
        let (_temp, repo) = repo_with_config("pg-c1");
        let (uc, _c) = use_case(MockCompute::default(), /* support_users */ false);
        let err = uc
            .grant(
                &repo,
                &grant_spec(
                    "app_ro",
                    GrantableObject::Table {
                        schema: "public".into(),
                        name: "t".into(),
                    },
                    vec![Privilege::Select],
                ),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ManageUsersError::Unsupported(_)));
    }
}
