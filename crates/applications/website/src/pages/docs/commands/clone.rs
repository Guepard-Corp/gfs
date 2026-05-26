use crate::components::CodeBlock;
use leptos::*;

#[component]
pub fn CommandClone() -> impl IntoView {
    view! {
        <div>
            <h1>"gfs clone"</h1>
            <p class="lead">"Lazily clone a remote PostgreSQL database (copy-on-read). Experimental."</p>

            <p>
                "Clone a remote database "<strong>"instantly"</strong>": only the schema is mirrored up front, "
                "no data is moved. Reads are served live from the remote until rows are written or warmed "
                "locally; "<strong>"writes always stay local"</strong>", so the clone diverges from the remote "
                "(Git's "<code>"clone"</code>" semantics for databases). The remote is accessed "
                <strong>"read-only"</strong>" ("<code>"SELECT"</code>" only): nothing is created on it."
            </p>

            <h2>"Usage"</h2>
            <CodeBlock code="gfs clone --from postgres://user:password@host:5432/dbname [PATH]"/>

            <h2>"Options"</h2>
            <ul>
                <li><code>"--from"</code>" (required) - Remote source URL: "<code>"postgres://user:password@host:port/dbname"</code>". Add "<code>"?schema=a,b"</code>" to mirror specific schemas (default: all non-system schemas)."</li>
                <li><code>"PATH"</code>" - Where to initialize the clone (default: current directory)."</li>
                <li><code>"--database-version"</code>" - Version for the local engine (e.g. 17). Omit to match the remote's major version automatically."</li>
                <li><code>"--image"</code>" - Override the local container image (e.g. "<code>"pgvector/pgvector:pg16"</code>"). Use when the source relies on an extension the default image lacks; pins its own version."</li>
                <li><code>"--platform"</code>" - Platform for the local container (e.g. "<code>"linux/amd64"</code>"), to run an image lacking a native-arch manifest (via emulation)."</li>
                <li><code>"--port"</code>" - Host port to bind the local database container."</li>
            </ul>

            <h2>"How it works"</h2>
            <p>
                "Each cloned table becomes an updatable "<strong>"view"</strong>" that unions a local store "
                "with the remote (via "<code>"postgres_fdw"</code>"), where local always wins. "
                "Selective reads push their predicate to the remote (only matching rows are fetched). "
                "Writes go through "<code>"INSTEAD OF"</code>" triggers into the local store; deletes are "
                "tombstoned. Correctness holds by construction: aggregates like "<code>"count(*)"</code>" are exact."
            </p>

            <h2>"Examples"</h2>
            <h3>"Clone a remote database"</h3>
            <CodeBlock code="gfs clone --from 'postgres://reader:secret@db.example.com:5432/shop' ./my-clone"/>

            <h3>"Source uses an extension (e.g. pgvector)"</h3>
            <CodeBlock code="gfs clone --from 'postgres://reader:secret@host:5432/shop' --image pgvector/pgvector:pg16"/>

            <h3>"Image without a native-arch build (Apple Silicon)"</h3>
            <CodeBlock code="gfs clone --from 'postgres://reader:secret@host:5432/shop' --image some/pg-image:18 --platform linux/amd64"/>

            <p>
                "Quote the URL in single quotes if the password contains shell metacharacters "
                "(e.g. a backtick)."
            </p>

            <h2>"Notes & limitations"</h2>
            <ul>
                <li>"Plain CRUD (SELECT/INSERT/UPDATE/DELETE) needs no application change."</li>
                <li>"Untouched rows are read live from the remote (they reflect the current source). Rows you write (or warm) are frozen locally and stop tracking the remote (local wins). So it is a copy-on-write overlay, not a snapshot or a follower; if the remote changes, untouched and locally-frozen rows can be from different points in time."</li>
                <li>"Cloned tables are views, so DDL ("<code>"ALTER TABLE"</code>", "<code>"CREATE INDEX"</code>", "<code>"TRUNCATE"</code>") and "<code>"SELECT … FOR UPDATE"</code>" are not supported on them; target the source or the local store."</li>
                <li>"Tables with no primary key or unique index are skipped."</li>
                <li>"Auto-increment works locally (sequences start past the remote max, so no key collisions)."</li>
            </ul>

            <h2>"See Also"</h2>
            <ul>
                <li><a href="/docs/commands/init">"gfs init"</a>" - Initialize a fresh repository"</li>
                <li><a href="/docs/commands/query">"gfs query"</a>" - Query the database"</li>
                <li><a href="/docs/commands/status">"gfs status"</a>" - Connection string and container status"</li>
            </ul>
        </div>
    }
}
