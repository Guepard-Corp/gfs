use leptos::*;
use leptos_router::*;

mod ai_agents;
mod coming_soon;
mod commands;
mod getting_started;
mod installation;
mod mcp;
mod quick_start;
mod telemetry;

use ai_agents::{AiAgentsSkills, AiAgentsSubagents};
use coming_soon::ComingSoon;
use commands::{
    CommandCheckout, CommandCommit, CommandCompute, CommandConfig, CommandExport, CommandImport,
    CommandInit, CommandLog, CommandProviders, CommandQuery, CommandSchema, CommandStatus,
};
use getting_started::GettingStarted;
use installation::Installation;
use mcp::{McpClaudeCode, McpClaudeDesktop, McpCursor, McpHttpMode, McpOverview};
use quick_start::QuickStart;
use telemetry::Telemetry;

#[component]
pub fn Docs() -> impl IntoView {
    let params = use_params_map();
    let page = move || {
        params.with(|p| {
            p.get("page")
                .cloned()
                .unwrap_or_else(|| "getting-started".to_string())
        })
    };

    view! {
        <div class="docs-page">
            <div class="container">
                <div class="docs-layout">
                    <aside class="docs-sidebar">
                        <nav class="docs-nav">
                            <div class="nav-section">
                                <h3>"Getting Started"</h3>
                                <ul>
                                    <li><A href="/docs" class="nav-item">"Introduction"</A></li>
                                    <li><A href="/docs/installation" class="nav-item">"Installation"</A></li>
                                    <li><A href="/docs/quick-start" class="nav-item">"Quick Start"</A></li>
                                </ul>
                            </div>
                            <div class="nav-section">
                                <h3>"Commands"</h3>
                                <ul>
                                    <li><A href="/docs/commands/init" class="nav-item">"gfs init"</A></li>
                                    <li><A href="/docs/commands/status" class="nav-item">"gfs status"</A></li>
                                    <li><A href="/docs/commands/commit" class="nav-item">"gfs commit"</A></li>
                                    <li><A href="/docs/commands/log" class="nav-item">"gfs log"</A></li>
                                    <li><A href="/docs/commands/checkout" class="nav-item">"gfs checkout"</A></li>
                                    <li><A href="/docs/commands/providers" class="nav-item">"gfs providers"</A></li>
                                    <li><A href="/docs/commands/query" class="nav-item">"gfs query"</A></li>
                                    <li><A href="/docs/commands/schema" class="nav-item">"gfs schema"</A></li>
                                    <li><A href="/docs/commands/export" class="nav-item">"gfs export"</A></li>
                                    <li><A href="/docs/commands/import" class="nav-item">"gfs import"</A></li>
                                    <li><A href="/docs/commands/config" class="nav-item">"gfs config"</A></li>
                                    <li><A href="/docs/commands/compute" class="nav-item">"gfs compute"</A></li>
                                </ul>
                            </div>
                            <div class="nav-section">
                                <h3>"MCP Server"</h3>
                                <ul>
                                    <li><A href="/docs/mcp/overview" class="nav-item">"Overview"</A></li>
                                    <li><A href="/docs/mcp/claude-desktop" class="nav-item">"Claude Desktop"</A></li>
                                    <li><A href="/docs/mcp/claude-code" class="nav-item">"Claude Code"</A></li>
                                    <li><A href="/docs/mcp/cursor" class="nav-item">"Cursor"</A></li>
                                    <li><A href="/docs/mcp/http-mode" class="nav-item">"HTTP Mode"</A></li>
                                </ul>
                            </div>
                            <div class="nav-section">
                                <h3>"AI Agents"</h3>
                                <ul>
                                    <li><A href="/docs/ai-agents/skills" class="nav-item">"Skills"</A></li>
                                    <li><A href="/docs/ai-agents/subagents" class="nav-item">"Subagents"</A></li>
                                </ul>
                            </div>
                            <div class="nav-section">
                                <h3>"Advanced"</h3>
                                <ul>
                                    <li><A href="/docs/configuration" class="nav-item">"Configuration"</A></li>
                                    <li><A href="/docs/troubleshooting" class="nav-item">"Troubleshooting"</A></li>
                                    <li><A href="/docs/development" class="nav-item">"Development"</A></li>
                                    <li><A href="/docs/telemetry" class="nav-item">"Telemetry"</A></li>
                                </ul>
                            </div>
                        </nav>
                    </aside>
                    <article class="docs-content">
                        {move || match page().as_str() {
                            "" | "getting-started" => view! { <GettingStarted/> }.into_view(),
                            "installation" => view! { <Installation/> }.into_view(),
                            "quick-start" => view! { <QuickStart/> }.into_view(),
                            "commands/init" => view! { <CommandInit/> }.into_view(),
                            "commands/status" => view! { <CommandStatus/> }.into_view(),
                            "commands/commit" => view! { <CommandCommit/> }.into_view(),
                            "commands/log" => view! { <CommandLog/> }.into_view(),
                            "commands/checkout" => view! { <CommandCheckout/> }.into_view(),
                            "commands/providers" => view! { <CommandProviders/> }.into_view(),
                            "commands/query" => view! { <CommandQuery/> }.into_view(),
                            "commands/schema" => view! { <CommandSchema/> }.into_view(),
                            "commands/export" => view! { <CommandExport/> }.into_view(),
                            "commands/import" => view! { <CommandImport/> }.into_view(),
                            "commands/config" => view! { <CommandConfig/> }.into_view(),
                            "commands/compute" => view! { <CommandCompute/> }.into_view(),
                            "ai-agents/skills" => view! { <AiAgentsSkills/> }.into_view(),
                            "ai-agents/subagents" => view! { <AiAgentsSubagents/> }.into_view(),
                            "mcp/overview" => view! { <McpOverview/> }.into_view(),
                            "mcp/claude-desktop" => view! { <McpClaudeDesktop/> }.into_view(),
                            "mcp/claude-code" => view! { <McpClaudeCode/> }.into_view(),
                            "mcp/cursor" => view! { <McpCursor/> }.into_view(),
                            "mcp/http-mode" => view! { <McpHttpMode/> }.into_view(),
                            "telemetry" => view! { <Telemetry/> }.into_view(),
                            _ => view! { <ComingSoon page=page()/> }.into_view(),
                        }}
                    </article>
                </div>
            </div>
        </div>
    }
}
