use crate::components::CodeBlock;
use leptos::*;

#[component]
pub fn SchemaDiffTabs() -> impl IntoView {
    let (active_tab, set_active_tab) = create_signal("default");

    view! {
        <div class="schema-diff-tabs">
            <div class="tabs-header">
                <button
                    class=move || if active_tab.get() == "default" { "tab active" } else { "tab" }
                    on:click=move |_| set_active_tab.set("default")
                >
                    "Default (agentic)"
                </button>
                <button
                    class=move || if active_tab.get() == "pretty" { "tab active" } else { "tab" }
                    on:click=move |_| set_active_tab.set("pretty")
                >
                    "Pretty"
                </button>
                <button
                    class=move || if active_tab.get() == "json" { "tab active" } else { "tab" }
                    on:click=move |_| set_active_tab.set("json")
                >
                    "JSON"
                </button>
            </div>
            <div class="tabs-content">
                {move || match active_tab.get() {
                    "default" => view! {
                        <div class="tab-panel">
                            <p class="tab-use-case">
                                "Best for: AI agents, scripts, and CI pipelines. Line-oriented format with one mutation per line. "
                                "Deterministic, parseable, no colors. Ideal when piping output or feeding into other tools."
                            </p>
                            <h4>"Command"</h4>
                            <CodeBlock code="gfs schema diff a3f1c2 b7d4e9"/>
                            <h4>"Example output"</h4>
                            <pre class="example-output"><code>"GFS_DIFF v1 from=a3f1c2 to=b7d4e9 breaking=false
COLUMN ADD public.users.verified_at type=timestamp nullable=true
COLUMN MODIFY public.users.email type=varchar(100)->varchar(255)"</code></pre>
                        </div>
                    }.into_view(),
                    "pretty" => view! {
                        <div class="tab-panel">
                            <p class="tab-use-case">
                                "Best for: human review, pull requests, and terminal inspection. "
                                "Visual tree with colors (green for adds, red for drops, yellow for changes). "
                                "Shows summary and breaking-change warnings."
                            </p>
                            <h4>"Command"</h4>
                            <CodeBlock code="gfs schema diff a3f1c2 b7d4e9 --pretty"/>
                            <h4>"Example output"</h4>
                            <pre class="example-output"><code>"Schema diff  main@a3f1c2 → main@b7d4e9
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
  MODIFIED    users
  │  + column   verified_at   timestamp   nullable
  │  ~ column   email   varchar(100) → varchar(255)
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
  Summary   1 modified
  Risk      ✓ Safe changes"</code></pre>
                        </div>
                    }.into_view(),
                    "json" => view! {
                        <div class="tab-panel">
                            <p class="tab-use-case">
                                "Best for: APIs, CI/CD integrations, and tooling. "
                                "Structured JSON with version, commits, mutations, and summary stats. "
                                "Easy to parse programmatically or feed into custom workflows."
                            </p>
                            <h4>"Command"</h4>
                            <CodeBlock code="gfs schema diff a3f1c2 b7d4e9 --json"/>
                            <h4>"Example output"</h4>
                            <pre class="example-output"><code>"{
  \"version\": \"1\",
  \"from_commit\": \"a3f1c2\",
  \"to_commit\": \"b7d4e9\",
  \"has_breaking_changes\": false,
  \"exit_code\": 1,
  \"mutations\": [
    {
      \"entity\": \"Column\",
      \"operation\": \"Add\",
      \"target\": \"public.users.verified_at\",
      \"metadata\": { \"type\": \"timestamp\", \"nullable\": \"true\" },
      \"is_breaking\": false
    },
    {
      \"entity\": \"Column\",
      \"operation\": \"Modify\",
      \"target\": \"public.users.email\",
      \"metadata\": { \"type\": \"varchar(100)->varchar(255)\" },
      \"is_breaking\": false
    }
  ],
  \"summary\": {
    \"total\": 2,
    \"by_operation\": { \"Add\": 1, \"Modify\": 1 },
    \"by_entity\": { \"Column\": 2 }
  }
}"</code></pre>
                        </div>
                    }.into_view(),
                    _ => view! { <div></div> }.into_view(),
                }}
            </div>
        </div>
    }
}
