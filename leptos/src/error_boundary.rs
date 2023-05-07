use crate::Children;
use leptos_dom::{Errors, IntoView};
use leptos_macro::{component, view};
use leptos_reactive::{
    create_rw_signal, provide_context, signal_prelude::*, RwSignal, Scope,
};

/// When you render a `Result<_, _>` in your view, in the `Err` case it will
/// render nothing, and search up through the view tree for an `<ErrorBoundary/>`.
/// This component lets you define a fallback that should be rendered in that
/// error case, allowing you to handle errors within a section of the interface.
///
/// ```
/// # use leptos_reactive::*;
/// # use leptos_macro::*;
/// # use leptos_dom::*; use leptos::*;
/// # run_scope(create_runtime(), |cx| {
/// let (value, set_value) = create_signal( Ok(0));
/// let on_input = move |ev| set_value(event_target_value(&ev).parse::<i32>());
///
/// view! { 
///   <input type="text" on:input=on_input/>
///   <ErrorBoundary
///     fallback=move |_, _| view! {  <p class="error">"Enter a valid number."</p>}
///   >
///     <p>"Value is: " {value}</p>
///   </ErrorBoundary>
/// }
/// # });
/// ```
#[component(transparent)]
pub fn ErrorBoundary<F, IV>(
    /// The components inside the tag which will get rendered
    children: Children,
    /// A fallback that will be shown if an error occurs.
    fallback: F,
) -> impl IntoView
where
    F: Fn(RwSignal<Errors>) -> IV + 'static,
    IV: IntoView,
{
    let errors: RwSignal<Errors> = create_rw_signal( Errors::default());

    provide_context( errors);

    // Run children so that they render and execute resources
    let children = children().into_view();
    let errors_empty = create_memo( move |_| errors.with(Errors::is_empty));

    move || {
        if errors_empty.get() {
            children.clone().into_view()
        } else {
            view! { 
                <>
                    {fallback( errors)}
                    <leptos-error-boundary style="display: none">{children.clone()}</leptos-error-boundary>
                </>
            }
            .into_view()
        }
    }
}
