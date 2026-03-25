use std::sync::Arc;

use crate::index::IndexView;

pub(crate) fn resolve_enclosing_owner_internal(
    view: Option<&IndexView>,
    enclosing_internal_name: Option<&str>,
    enclosing_package: Option<&str>,
    enclosing_class_chain: &[Arc<str>],
    simple_name: &str,
) -> Option<Arc<str>> {
    let simple_name = simple_name.trim();
    if simple_name.is_empty() {
        return None;
    }

    resolve_owner_from_chain(enclosing_package, enclosing_class_chain, simple_name)
        .or_else(|| resolve_owner_from_index(view, enclosing_internal_name, simple_name))
}

fn resolve_owner_from_chain(
    enclosing_package: Option<&str>,
    enclosing_class_chain: &[Arc<str>],
    simple_name: &str,
) -> Option<Arc<str>> {
    let target_depth = enclosing_class_chain
        .iter()
        .enumerate()
        .rev()
        .find_map(|(idx, name)| (name.as_ref() == simple_name).then_some(idx))?;

    let mut internal = String::new();
    if let Some(pkg) = enclosing_package
        && !pkg.is_empty()
    {
        internal.push_str(pkg);
        internal.push('/');
    }

    let mut segments = enclosing_class_chain.iter().take(target_depth + 1);
    let first = segments.next()?;
    internal.push_str(first.as_ref());
    for segment in segments {
        internal.push('$');
        internal.push_str(segment.as_ref());
    }

    Some(Arc::from(internal))
}

fn resolve_owner_from_index(
    view: Option<&IndexView>,
    enclosing_internal_name: Option<&str>,
    simple_name: &str,
) -> Option<Arc<str>> {
    let view = view?;
    let enclosing_internal_name = enclosing_internal_name?;
    let mut current = view.get_class(enclosing_internal_name)?;

    loop {
        if current.matches_simple_name(simple_name) {
            return Some(Arc::clone(&current.internal_name));
        }
        current = view.resolve_owner_class(current.internal_name.as_ref())?;
    }
}

#[cfg(test)]
mod tests {
    use super::resolve_enclosing_owner_internal;
    use std::sync::Arc;

    #[test]
    fn test_resolve_enclosing_owner_internal_uses_exact_class_chain_for_dollar_names() {
        let chain = vec![Arc::from("Outer$Class"), Arc::from("Inner$Class")];

        assert_eq!(
            resolve_enclosing_owner_internal(
                None,
                Some("com/example/Outer$Class$Inner$Class"),
                Some("com/example"),
                &chain,
                "Outer$Class",
            )
            .as_deref(),
            Some("com/example/Outer$Class")
        );
        assert_eq!(
            resolve_enclosing_owner_internal(
                None,
                Some("com/example/Outer$Class$Inner$Class"),
                Some("com/example"),
                &chain,
                "Inner$Class",
            )
            .as_deref(),
            Some("com/example/Outer$Class$Inner$Class")
        );
    }
}
