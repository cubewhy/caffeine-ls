use crate::index::IndexView;
use crate::language::java::type_ctx::SourceTypeCtx;
use crate::semantic::context::{
    CursorLocation, JavaAccessReceiverKind, JavaIntrinsicAccess, JavaIntrinsicAccessKind,
    SemanticContext,
};
use crate::semantic::types::symbol_resolver::SymbolResolver;
use crate::semantic::types::type_name::TypeName;

const CLASS_LITERAL_KEYWORD: &str = "class";
const ARRAY_LENGTH_NAME: &str = "length";
const OBJECT_GET_CLASS_NAME: &str = "getClass";
const JAVA_LANG_CLASS_INTERNAL: &str = "java/lang/Class";

pub fn classify_intrinsic_access(
    ctx: &SemanticContext,
    view: &IndexView,
    type_ctx: &SourceTypeCtx,
) -> Option<JavaIntrinsicAccess> {
    match &ctx.location {
        CursorLocation::StaticAccess { member_prefix, .. } => {
            class_literal_access_for_prefix(member_prefix)
        }
        CursorLocation::MemberAccess {
            receiver_semantic_type,
            receiver_expr,
            member_prefix,
            arguments,
            ..
        } => {
            if arguments.is_none()
                && class_literal_access_for_member_receiver(ctx, view, type_ctx, receiver_expr)
                    .is_some()
                && is_class_literal_prefix(member_prefix)
            {
                return Some(JavaIntrinsicAccess {
                    kind: JavaIntrinsicAccessKind::ClassLiteral,
                    receiver_kind: JavaAccessReceiverKind::Type,
                });
            }

            if member_prefix == ARRAY_LENGTH_NAME
                && receiver_semantic_type
                    .as_ref()
                    .is_some_and(|ty| ty.is_array())
            {
                return Some(JavaIntrinsicAccess {
                    kind: JavaIntrinsicAccessKind::ArrayLength,
                    receiver_kind: JavaAccessReceiverKind::Expression,
                });
            }

            if member_prefix == OBJECT_GET_CLASS_NAME
                && arguments.as_deref() == Some("()")
                && receiver_semantic_type
                    .as_ref()
                    .is_some_and(|ty| !ty.is_primitive() && ty.erased_internal() != "null")
            {
                return Some(JavaIntrinsicAccess {
                    kind: JavaIntrinsicAccessKind::ObjectGetClass,
                    receiver_kind: JavaAccessReceiverKind::Expression,
                });
            }

            None
        }
        _ => None,
    }
}

pub fn is_class_literal_prefix(prefix: &str) -> bool {
    CLASS_LITERAL_KEYWORD.starts_with(prefix)
}

pub fn class_literal_result_type(view: &IndexView, operand: TypeName) -> TypeName {
    // Source-level typing keeps the class-literal operand precise, including arrays,
    // primitives, and `void`. If `java.lang.Class` is unavailable in the index, keep
    // the stable JDK internal name instead of degrading to raw `Class`.
    let class_internal = if view.get_class(JAVA_LANG_CLASS_INTERNAL).is_some() {
        JAVA_LANG_CLASS_INTERNAL
    } else {
        JAVA_LANG_CLASS_INTERNAL
    };
    TypeName::with_args(class_internal, vec![operand])
}

fn class_literal_access_for_prefix(member_prefix: &str) -> Option<JavaIntrinsicAccess> {
    if !is_class_literal_prefix(member_prefix) {
        return None;
    }

    Some(JavaIntrinsicAccess {
        kind: JavaIntrinsicAccessKind::ClassLiteral,
        receiver_kind: JavaAccessReceiverKind::Type,
    })
}

fn class_literal_access_for_member_receiver(
    ctx: &SemanticContext,
    view: &IndexView,
    type_ctx: &SourceTypeCtx,
    receiver_expr: &str,
) -> Option<()> {
    let receiver_expr = receiver_expr.trim();
    if receiver_expr.is_empty() {
        return None;
    }

    let mut base = receiver_expr;
    while let Some(stripped) = base.strip_suffix("[]") {
        base = stripped.trim();
    }

    if matches!(
        base,
        "boolean" | "byte" | "char" | "double" | "float" | "int" | "long" | "short" | "void"
    ) {
        return Some(());
    }

    if base.contains('<') || base.contains('>') {
        return None;
    }

    if !base
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$' || c == '.')
    {
        return None;
    }

    let parts: Vec<&str> = base.split('.').filter(|part| !part.is_empty()).collect();
    if parts.is_empty() {
        return None;
    }

    if parts[0] == "this"
        || ctx
            .local_variables
            .iter()
            .any(|lv| lv.name.as_ref() == parts[0])
    {
        return None;
    }

    let resolver = SymbolResolver::new(view);
    if resolver.resolve_type_name(ctx, base).is_some() {
        return Some(());
    }

    type_ctx.resolve_type_name_strict(base).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{ClassMetadata, ClassOrigin, IndexScope, ModuleId, WorkspaceIndex};
    use crate::semantic::LocalVar;
    use crate::semantic::types::type_name::TypeName;
    use std::sync::Arc;

    fn root_scope() -> IndexScope {
        IndexScope {
            module: ModuleId::ROOT,
        }
    }

    fn make_type_ctx(view: &IndexView) -> SourceTypeCtx {
        SourceTypeCtx::new(
            None,
            vec!["java.lang.*".into()],
            Some(view.build_name_table()),
        )
    }

    fn make_index() -> WorkspaceIndex {
        let idx = WorkspaceIndex::new();
        idx.add_classes(vec![
            ClassMetadata {
                package: Some(Arc::from("java/lang")),
                name: Arc::from("Object"),
                internal_name: Arc::from("java/lang/Object"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![],
                fields: vec![],
                access_flags: 0,
                inner_class_of: None,
                generic_signature: None,
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("java/lang")),
                name: Arc::from("String"),
                internal_name: Arc::from("java/lang/String"),
                super_name: Some(Arc::from("java/lang/Object")),
                interfaces: vec![],
                annotations: vec![],
                methods: vec![],
                fields: vec![],
                access_flags: 0,
                inner_class_of: None,
                generic_signature: None,
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("pkg")),
                name: Arc::from("Outer"),
                internal_name: Arc::from("pkg/Outer"),
                super_name: Some(Arc::from("java/lang/Object")),
                interfaces: vec![],
                annotations: vec![],
                methods: vec![],
                fields: vec![],
                access_flags: 0,
                inner_class_of: None,
                generic_signature: None,
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("pkg")),
                name: Arc::from("Inner"),
                internal_name: Arc::from("pkg/Outer$Inner"),
                super_name: Some(Arc::from("java/lang/Object")),
                interfaces: vec![],
                annotations: vec![],
                methods: vec![],
                fields: vec![],
                access_flags: 0,
                inner_class_of: Some(Arc::from("Outer")),
                generic_signature: None,
                origin: ClassOrigin::Unknown,
            },
        ]);
        idx
    }

    #[test]
    fn classifies_class_literal_for_static_access() {
        let idx = make_index();
        let view = idx.view(root_scope());
        let type_ctx = make_type_ctx(&view);
        let ctx = SemanticContext::new(
            CursorLocation::StaticAccess {
                class_internal_name: Arc::from("java/lang/String"),
                member_prefix: "cl".to_string(),
            },
            "cl",
            vec![],
            None,
            None,
            None,
            vec![],
        );

        assert_eq!(
            classify_intrinsic_access(&ctx, &view, &type_ctx),
            Some(JavaIntrinsicAccess {
                kind: JavaIntrinsicAccessKind::ClassLiteral,
                receiver_kind: JavaAccessReceiverKind::Type,
            })
        );
    }

    #[test]
    fn classifies_class_literal_for_primitive_and_array_type_receivers() {
        let idx = make_index();
        let view = idx.view(root_scope());
        let type_ctx = make_type_ctx(&view);
        let primitive_ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_semantic_type: None,
                receiver_type: None,
                member_prefix: "".to_string(),
                receiver_expr: "int".to_string(),
                arguments: None,
            },
            "",
            vec![],
            None,
            None,
            None,
            vec![],
        );
        let array_ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_semantic_type: None,
                receiver_type: None,
                member_prefix: "cl".to_string(),
                receiver_expr: "String[]".to_string(),
                arguments: None,
            },
            "cl",
            vec![],
            None,
            None,
            None,
            vec![],
        );

        assert_eq!(
            classify_intrinsic_access(&primitive_ctx, &view, &type_ctx).map(|a| a.kind),
            Some(JavaIntrinsicAccessKind::ClassLiteral)
        );
        assert_eq!(
            classify_intrinsic_access(&array_ctx, &view, &type_ctx).map(|a| a.kind),
            Some(JavaIntrinsicAccessKind::ClassLiteral)
        );
    }

    #[test]
    fn rejects_class_literal_for_value_receivers_and_parameterized_types() {
        let idx = make_index();
        let view = idx.view(root_scope());
        let type_ctx = make_type_ctx(&view);
        let value_ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_semantic_type: Some(TypeName::new("java/lang/String")),
                receiver_type: Some(Arc::from("java/lang/String")),
                member_prefix: "".to_string(),
                receiver_expr: "obj".to_string(),
                arguments: None,
            },
            "",
            vec![LocalVar {
                name: Arc::from("obj"),
                type_internal: TypeName::new("java/lang/String"),
                init_expr: None,
            }],
            None,
            None,
            None,
            vec![],
        );
        let generic_ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_semantic_type: None,
                receiver_type: None,
                member_prefix: "cl".to_string(),
                receiver_expr: "List<String>".to_string(),
                arguments: None,
            },
            "cl",
            vec![],
            None,
            None,
            None,
            vec![],
        );

        assert_eq!(
            classify_intrinsic_access(&value_ctx, &view, &type_ctx),
            None
        );
        assert_eq!(
            classify_intrinsic_access(&generic_ctx, &view, &type_ctx),
            None
        );
    }

    #[test]
    fn classifies_array_length_and_object_get_class_separately() {
        let idx = make_index();
        let view = idx.view(root_scope());
        let type_ctx = make_type_ctx(&view);
        let array_ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_semantic_type: Some(TypeName::new("java/lang/String").with_array_dims(1)),
                receiver_type: Some(Arc::from("java/lang/String")),
                member_prefix: "length".to_string(),
                receiver_expr: "arr".to_string(),
                arguments: None,
            },
            "length",
            vec![],
            None,
            None,
            None,
            vec![],
        );
        let get_class_ctx = SemanticContext::new(
            CursorLocation::MemberAccess {
                receiver_semantic_type: Some(TypeName::new("java/lang/String")),
                receiver_type: Some(Arc::from("java/lang/String")),
                member_prefix: "getClass".to_string(),
                receiver_expr: "obj".to_string(),
                arguments: Some("()".to_string()),
            },
            "getClass",
            vec![],
            None,
            None,
            None,
            vec![],
        );

        assert_eq!(
            classify_intrinsic_access(&array_ctx, &view, &type_ctx).map(|a| a.kind),
            Some(JavaIntrinsicAccessKind::ArrayLength)
        );
        assert_eq!(
            classify_intrinsic_access(&get_class_ctx, &view, &type_ctx).map(|a| a.kind),
            Some(JavaIntrinsicAccessKind::ObjectGetClass)
        );
    }
}
