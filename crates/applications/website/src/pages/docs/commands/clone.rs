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
                "no data is moved. Data is fetched from the remote the first time a query needs it "
                "("<strong>"copy-on-read"</strong>") and kept locally; "<strong>"writes always stay local"</strong>", "
                "so the clone diverges from the remote (Git's "<code>"clone"</code>" semantics for databases). "
                "The remote is accessed "<strong>"read-only"</strong>" ("<code>"SELECT"</code>" only): nothing is "
                "created on it."
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
                "Each cloned table is a "<strong>"real, empty table"</strong>" with the source's schema and "
                "indexes — the app cannot tell the clone from an ordinary database. An embedded PostgreSQL "
                "extension (a planner hook) routes every query with a cost model:"
            </p>
            <ul>
                <li><strong>"Hydrate"</strong>" - a read that bounds the table's key fetches the missing rows once into the local table, then runs on local indexes. Asking again touches no source."</li>
                <li><strong>"Partial"</strong>" - a selective predicate on a table too big to copy whole fetches only the matching slice (capped); a repeat is served locally."</li>
                <li><strong>"Federate"</strong>" - joins and aggregates with no key bound are pushed whole to the source (via "<code>"postgres_fdw"</code>"); only the result comes back, nothing is materialized locally."</li>
                <li><strong>"Owned"</strong>" - once a table is fully materialized (reads filled it in, or it was force-warmed), it is served locally forever - no source contact."</li>
            </ul>
            <p>
                "The router's cost weights are "<strong>"measured"</strong>" from the source link at clone "
                "time, and a per-clone token bucket rate-limits source contact so many clones cannot "
                "overwhelm production. Correctness holds by construction: a scan is served locally only "
                "when its rows are provably present, otherwise it reads the source - never a partial "
                "result. Aggregates like "<code>"count(*)"</code>" are exact."
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
                <li>"Plain CRUD (SELECT/INSERT/UPDATE/DELETE) needs no application change, and cloned tables are real tables: indexes and writes behave normally."</li>
                <li>"Rows are frozen locally once read, written or warmed, and stop tracking the remote; untouched rows still reflect the live source. So it is a lazy working copy, not a snapshot or a follower: if the remote changes, fetched and untouched rows can be from different points in time."</li>
                <li>"Foreign keys are dropped on the clone: rows arrive lazily table by table (a child can land before its parent), and the source already enforced them."</li>
                <li>"Tables with no primary key or unique index are skipped."</li>
                <li>"Tables whose types need an extension the local image lacks are skipped - pass "<code>"--image"</code>" with an image that ships it."</li>
                <li>"Auto-increment works locally (sequences start past the remote max, so no key collisions)."</li>
                <li>"There is deliberately no "<code>"gfs push"</code>" and never will be: the source is read-only by hard rule. "<a href="/docs/commands/fetch">"gfs fetch"</a>" and "<a href="/docs/commands/pull">"gfs pull"</a>" are the sync verbs that do exist."</li>
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
