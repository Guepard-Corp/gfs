use leptos::*;

#[component]
pub fn CodeBlock(#[prop(into)] code: String) -> impl IntoView {
    view! {
        <div class="code-block-wrapper">
            <pre>
                <code>{code.clone()}</code>
                <button
                    class="copy-button"
                    title="Copy to clipboard"
                    data-code=code
                >
                    <svg width="20" height="20" viewBox="0 0 24 24" fill="none" xmlns="http://www.w3.org/2000/svg">
                        <path d="M8.75 8.75V2.75H21.25V15.25H15.25M15.25 8.75H2.75V21.25H15.25V8.75Z" stroke="currentColor" stroke-width="1.5" stroke-linecap="square"></path>
                    </svg>
                </button>
            </pre>
        </div>
    }
}
