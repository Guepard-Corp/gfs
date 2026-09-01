use crate::components::CodeBlock;
use leptos::*;

#[component]
pub fn CommandStatus() -> impl IntoView {
    view! {
        <div>
            <h1>"gfs status"</h1>
            <p class="lead">"Show the current state of storage and compute resources."</p>

            <h2>"Usage"</h2>
            <CodeBlock code="gfs status"/>

            <h2>"Description"</h2>
            <p>"The "<code>"status"</code>" command displays information about your GFS repository, including:"</p>
            <ul>
                <li>"Current branch"</li>
                <li>"Database connection information"</li>
                <li>"Docker container status"</li>
                <li>"Storage backend information"</li>
                <li>"Compute resource status"</li>
                <li>"On a lazy clone: a "<strong>"Source"</strong>" section — tracked tables, how many are behind the source, how many diverged (changed both locally and upstream), and when that verdict was last checked"</li>
            </ul>
            <p>
                "The Source section is a "<em>"cached"</em>" verdict: like "<code>"git status"</code>", "
                "the command never contacts the source. Run "<code>"gfs fetch --check"</code>" to "
                "probe it now."
            </p>

            <h2>"Example Output"</h2>
            <pre><code>"  Repository\n  ────────────────────────────────────────\n  Branch               main\n  Active workspace     .gfs/workspaces/main/0/data\n\n  Compute\n  ────────────────────────────────────────\n  Provider             postgres\n  Version              17\n  Status               ● running\n  Container ID         37f65464d421…\n  Container data dir   .gfs/workspaces/main/0/data\n  Connection           postgresql://postgres:postgres@localhost:55251/postgres\n\n  Source\n  ────────────────────────────────────────\n  Tracked tables       12\n  Behind               3\n  Diverged             1\n  Checked              2026-08-31 14:02:11"</code></pre>

            <h2>"JSON output"</h2>
            <CodeBlock code="gfs status --output json"/>
            <p>
                "On a lazy clone the JSON carries the same data as a "<code>"source"</code>" object: "
                <code>"tracked"</code>", "<code>"behind"</code>", "<code>"diverged"</code>", "
                <code>"last_checked"</code>" (named to match "<code>"gfs fetch --json"</code>"). "
                "The object is omitted entirely when the repository is not a clone, so existing "
                "consumers see no change."
            </p>

            <h2>"Use Cases"</h2>
            <ul>
                <li>"Check if the database container is running"</li>
                <li>"Get connection details for your database"</li>
                <li>"Verify which branch you're currently on"</li>
                <li>"Troubleshoot connection issues"</li>
            </ul>

            <h2>"See Also"</h2>
            <ul>
                <li><a href="/docs/commands/init">"gfs init"</a>" - Initialize a repository"</li>
                <li><a href="/docs/commands/log">"gfs log"</a>" - View commit history"</li>
                <li><a href="/docs/commands/fetch">"gfs fetch"</a>" - Probe the source a clone reads from"</li>
                <li><a href="/docs/commands/pull">"gfs pull"</a>" - Bring a clone up to date"</li>
            </ul>
        </div>
    }
}
