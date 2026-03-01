use crate::components::CodeBlock;
use leptos::*;

#[component]
pub fn CodeTabs() -> impl IntoView {
    let (active_tab, set_active_tab) = create_signal("curl");

    view! {
        <section class="install-section">
            <div class="container">
                <h2 class="section-title">"Get Started in Seconds"</h2>
                <div class="code-tabs">
                    <div class="tabs-header">
                        <button
                            class=move || if active_tab.get() == "curl" { "tab active" } else { "tab" }
                            on:click=move |_| set_active_tab.set("curl")
                        >
                            "curl"
                        </button>
                        <button
                            class=move || if active_tab.get() == "brew" { "tab active" } else { "tab" }
                            on:click=move |_| set_active_tab.set("brew")
                        >
                            "Homebrew"
                        </button>
                        <button
                            class=move || if active_tab.get() == "cargo" { "tab active" } else { "tab" }
                            on:click=move |_| set_active_tab.set("cargo")
                        >
                            "Cargo"
                        </button>
                    </div>
                    <div class="code-content">
                        {move || match active_tab.get() {
                            "curl" => view! {
                                <CodeBlock code="curl -fsSL https://gfs.guepard.run/install | bash".to_string()/>
                            }.into_view(),
                            "brew" => view! {
                                <CodeBlock code="brew install guepard-corp/tap/gfs".to_string()/>
                            }.into_view(),
                            "cargo" => view! {
                                <CodeBlock code="# Build from source\ngit clone https://github.com/Guepard-Corp/gfs.git\ncd gfs\ncargo build --release".to_string()/>
                            }.into_view(),
                            _ => view! { <CodeBlock code="".to_string()/> }.into_view(),
                        }}
                    </div>
                </div>
            </div>
        </section>
    }
}
