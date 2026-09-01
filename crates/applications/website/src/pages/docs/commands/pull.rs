use crate::components::CodeBlock;
use leptos::*;

#[component]
pub fn CommandPull() -> impl IntoView {
    view! {
        <div>
            <h1>"gfs pull"</h1>
            <p class="lead">"Put tables the source has changed back on the lazy path, so reads are local (and fast) again."</p>

            <h2>"Usage"</h2>
            <CodeBlock code="gfs pull [--force] [--auto on|off] [--auto-schema on|off]"/>

            <h2>"Description"</h2>
            <p>
                <code>"pull"</code>" copies nothing itself: it clears cached state so the next read "
                "refetches, and the normal cost model decides copy-vs-federate exactly as it would "
                "for a fresh clone. It is never required for correctness — a clone that is never "
                "pulled is slow, not wrong — it exists to make reads local again."
            </p>
            <p>
                "A table you have written to is "<strong>"never"</strong>" reset without "
                <code>"--force"</code>": it is reported as a conflict, the way git refuses to "
                "clobber local changes."
            </p>

            <h2>"Options"</h2>
            <ul>
                <li><code>"--force"</code>" - Also reset tables you have written to, discarding those local changes."</li>
                <li><code>"--auto on|off"</code>" - Turn automatic pulling on or off instead of pulling now."</li>
                <li><code>"--auto-schema on|off"</code>" - Turn automatic schema repair on or off instead of pulling now. A column dropped on the source is never applied automatically."</li>
                <li><code>"--path"</code>" - Path to the GFS repository root (default: current directory)."</li>
                <li><code>"--json"</code>" - JSON output: reset and conflict counts plus per-table actions."</li>
            </ul>

            <h2>"There is no gfs push — deliberately"</h2>
            <p>
                "The sync verbs are asymmetric: "<code>"fetch"</code>" and "<code>"pull"</code>" "
                "exist, "<code>"push"</code>" never will. The source is typically a production "
                "database; pushing a clone's test data into it is exactly the foot-gun GFS exists "
                "to prevent. \"The source is never written to\" is a hard rule of the system, not "
                "a missing feature. "<code>"gfs remote"</code>" shows the source and marks it "
                "fetch-only. Resolving diverged tables ("<code>"merge"</code>"/"<code>"rebase"</code>") "
                "is future work blocked on row-level change tracking."
            </p>

            <h2>"See Also"</h2>
            <ul>
                <li><a href="/docs/commands/fetch">"gfs fetch"</a>" - See what changed before acting on it"</li>
                <li><a href="/docs/commands/status">"gfs status"</a>" - Source summary alongside branch and compute info"</li>
                <li><a href="/docs/commands/clone">"gfs clone"</a>" - Create a lazy clone"</li>
            </ul>
        </div>
    }
}
