use crate::components::CodeBlock;
use leptos::*;

#[component]
pub fn CommandConfig() -> impl IntoView {
    view! {
        <div>
            <h1>"gfs config"</h1>
            <p class="lead">"Read or write repository or global config (user.name, user.email)."</p>

            <h2>"Usage"</h2>
            <CodeBlock code="# Read a config value (repo-local)
gfs config user.name

# Set a config value (repo-local)
gfs config user.name \"John Doe\"

# Read / set global config (~/.gfs/config.toml)
gfs config --global user.name
gfs config --global user.name \"John Doe\""/>

            <h2>"Options"</h2>
            <ul>
                <li><code>"--global, -g"</code>" - Operate on the global config ("
                    <code>"~/.gfs/config.toml"</code>
                    ") instead of the repo-local "
                    <code>".gfs/config.toml"</code>
                </li>
                <li><code>"--path"</code>" - Path to the GFS repository root (ignored with --global)"</li>
            </ul>

            <h2>"Supported Keys"</h2>
            <ul>
                <li><code>"user.name"</code>" - Author name used in commits"</li>
                <li><code>"user.email"</code>" - Author email used in commits"</li>
            </ul>

            <h2>"Config Scopes"</h2>
            <p>"GFS resolves identity in this order for every commit:"</p>
            <ol>
                <li>"CLI flags "<code>"--author"</code>" / "<code>"--author-email"</code></li>
                <li>"Repo-local "<code>".gfs/config.toml"</code>" "<code>"[user]"</code>" section"</li>
                <li>"Global "<code>"~/.gfs/config.toml"</code>" "<code>"[user]"</code>" section"</li>
                <li>"Git config ("<code>"git config user.name"</code>" / "<code>"user.email"</code>")"</li>
                <li>"Default: "<code>"\"user\""</code>" for name; email left unset"</li>
            </ol>

            <h2>"Examples"</h2>
            <h3>"Set global identity (all repos)"</h3>
            <CodeBlock code="gfs config --global user.name \"Alice\"
    gfs config --global user.email \"alice@example.com\""/>

            <h3>"Override identity for one repo"</h3>
            <CodeBlock code="gfs config user.name \"Alice\"
    gfs config user.email \"alice@example.com\""/>

            <h3>"Read back"</h3>
            <CodeBlock code="gfs config user.name          # repo-local
    gfs config --global user.name  # global"/>

            <h2>"Storage"</h2>
            <ul>
                <li>"Repo-local: "<code>".gfs/config.toml"</code>" under the "<code>"[user]"</code>" section"</li>
                <li>"Global: "<code>"~/.gfs/config.toml"</code>" under the "<code>"[user]"</code>" section (created on first write)"</li>
            </ul>

            <h2>"See Also"</h2>
            <ul>
                <li><a href="/docs/commands/commit">"gfs commit"</a>" - Uses user.name and user.email as default author"</li>
            </ul>
        </div>
    }
}
