use crate::components::CodeBlock;
use leptos::*;

#[component]
pub fn CommandFetch() -> impl IntoView {
    view! {
        <div>
            <h1>"gfs fetch"</h1>
            <p class="lead">"Show what has changed on a clone's source. Read-only: it never modifies the clone."</p>

            <h2>"Usage"</h2>
            <CodeBlock code="gfs fetch [--check] [--json]"/>

            <h2>"Description"</h2>
            <p>
                "The read-only half of the source-sync pair: "<code>"fetch"</code>" reports, "
                <a href="/docs/commands/pull">"gfs pull"</a>" acts. By default it prints the last "
                "cached verdict — no network round trip, so it is fast and works even while the "
                "source is unreachable. "<code>"--check"</code>" probes the source right now."
            </p>
            <ul>
                <li>"Per table: unchanged, "<strong>"changed"</strong>" (with the reason), or "<strong>"conflict"</strong>" — you have local writes AND the source changed."</li>
                <li>"Findings that belong to no tracked table are listed too: a table newly created on the source, or movement nothing accounts for."</li>
                <li>"A changed table is still read "<em>"correctly"</em>" — reads go to the source until you "<code>"gfs pull"</code>" — just slower."</li>
            </ul>
            <p>
                "There is no separate "<code>"gfs diff"</code>": GFS has no per-row change log on "
                "either side, so the verdict is table-granular and "<code>"fetch"</code>" already "
                "shows everything a diff could know."
            </p>

            <h2>"Options"</h2>
            <ul>
                <li><code>"--check"</code>" - Probe the source now instead of using the last cached verdict."</li>
                <li><code>"--path"</code>" - Path to the GFS repository root (default: current directory)."</li>
                <li><code>"--json"</code>" - JSON output: "<code>"tables_tracked"</code>", "<code>"tables_changed"</code>", "<code>"conflicts"</code>", "<code>"last_checked"</code>", plus per-table detail."</li>
            </ul>

            <h2>"See Also"</h2>
            <ul>
                <li><a href="/docs/commands/pull">"gfs pull"</a>" - Act on what fetch reports"</li>
                <li><a href="/docs/commands/status">"gfs status"</a>" - Source summary alongside branch and compute info"</li>
                <li><a href="/docs/commands/clone">"gfs clone"</a>" - Create a lazy clone"</li>
            </ul>
        </div>
    }
}
