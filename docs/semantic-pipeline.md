# Java Semantic Pipeline (Current Architecture)

## 1. Glossary

- `CursorLocation`
  - Meaning: syntax-level cursor classification from tree-sitter context (`MemberAccess`, `MethodReference`, `TypeAnnotation`, etc.).
  - Not: a type inference result or compatibility decision.

- `SemanticContext`
  - Meaning: request-scoped semantic container used by completion providers.
  - Includes syntax location, locals/imports, enclosing class/package, and enrichment outputs.
  - Not: a full program-wide type-checking model.

- `FunctionalTargetHint`
  - Meaning: syntax-derived hint that the current expression is target-typed by assignment/method-argument context.
  - Carries expected type source text, argument-position info, and expression shape (`method reference` / `lambda`).
  - Not: proof that a functional conversion is valid.

- `ExpectedType`
  - Meaning: normalized expected type propagated into expression context.
  - Includes `source` (`AssignmentRhs` or `MethodArgument`) and `confidence` (`Exact` / `Partial`).
  - Not: inferred runtime type of the expression itself.

- `TypedExpressionContext`
  - Meaning: enriched expression context attached to `SemanticContext`.
  - Carries `expected_type`, optional receiver type, and functional compatibility summary.
  - Not: final overload/type-inference output.

- `SamSignature`
  - Meaning: extracted single abstract method shape for a functional interface.
  - Contains method name, SAM parameter types, and SAM return type.
  - Not: a solved generic substitution model.

- `FunctionalCompat`
  - Meaning: shallow compatibility result between expression shape and target SAM.
  - Fields: `status`, `resolved_owner`, `resolved_return`.
  - Not: full JLS lambda/method-reference constraint solving.

- `TypeName`
  - Meaning: canonical semantic type representation used in semantic/completion paths.
  - Preserves base, generic args, and array depth structurally.
  - Not: raw source text or a single canonical JVM descriptor string.

## 2. Type Representation Layers

- Source type text
  - Example: `Function<? super T, ? extends K>`
  - Origin: AST/source context and hints.
  - Use: input to `SourceTypeCtx` strict/relaxed normalization.

- Canonical semantic type (`TypeName`)
  - Example: base + structured args + array dims.
  - Use: semantic propagation, expected type context, receiver typing.

- Erased/base owner for lookup
  - Example: `java/util/List`
  - Use: class/member lookup keys (`collect_inherited_members`, MRO).
  - Rule: never use serialized full generic internals as owner lookup keys.

- JVM descriptor/signature forms
  - Examples: `Ljava/util/List;`, `(Ljava/lang/Object;)I`, generic signatures.
  - Use: method parameter/return scoring and conversion helpers.
  - Rule: descriptors are matching/scoring artifacts, not primary semantic storage.

## 3. Enrichment Pipeline (Current)

1. Syntax location extraction
   - `location.rs` classifies cursor into `CursorLocation`.
   - For functional targets, `infer_functional_target_hint` captures:
     - assignment RHS expected type source
     - method argument target position
     - expression shape (`MethodReference` / `Lambda`).

2. Semantic context creation
   - `JavaContextExtractor` builds `SemanticContext` with locals/imports/class scope.

3. Receiver resolution + location normalization
   - `completion_context.rs` normalizes method-reference locations to existing routing paths.
   - Receiver semantic type is canonicalized via `TypeName` (strict normalization where possible).

4. Expected type propagation
   - `enrich_expected_type_context` computes `typed_expr_ctx.expected_type` from:
     - assignment RHS expected type source
     - method argument parameter type selection.
   - Confidence is `Exact` or `Partial`.

5. SAM extraction
   - If expected type exists and resolves to a functional interface, extract `expected_sam`.

6. Functional compatibility
   - If expression shape + expected SAM are both present:
     - evaluate shallow compatibility (`Exact` / `Partial` / `Incompatible`)
     - attach result to `typed_expr_ctx.functional_compat`.

## 4. Supported vs Not Yet Supported

- Supported now
  - Target expected type propagation for assignment RHS and method argument.
  - SAM extraction for single abstract method interfaces.
  - Shallow compatibility for:
    - `Type::method`
    - `expr::method`
    - `Type::new`
    - simple lambdas (parameter-count gate + minimal expression-body check).

- Not yet supported
  - Full JLS constraint solving for lambda/method-reference conversion.
  - Deep generic substitution/capture conversion across SAM and candidate methods.
  - Full lambda body inference (block body flow, statement-level typing).
  - Global overload-resolution redesign based on functional constraints.

## 5. Review Invariants

- Preserve representation boundaries
  - Keep source text, `TypeName`, erased owner, and descriptor/signature uses distinct.
  - Avoid implicit round-trips that collapse structure.

- Owner lookup invariant
  - Use erased owner (`TypeName::erased_internal()`) for class/member lookup keys.
  - Do not lookup class metadata using serialized generic internal strings.

- Expected type invariant
  - Prefer `typed_expr_ctx.expected_type` as the primary expected-type output.
  - Keep compatibility fields (`expected_functional_interface`, `expected_sam`) derived from this path.

- Relaxed resolution invariant
  - Preserve outer/base type on inner generic failures and mark `Partial`.
  - Do not drop to `None` when base type is known.

- Compatibility invariant
  - `Exact` only when clearly proven.
  - Use `Partial` for plausibly compatible but under-constrained cases.
  - Use `Incompatible` only when clearly wrong.

## 6. Anti-patterns to Avoid

- Provider-local ad hoc parsing that diverges from `SourceTypeCtx` normalization.
- Using descriptor strings as long-lived semantic type state.
- Mixing syntax classification concerns directly into provider ranking behavior.
- Treating functional compatibility output as final type inference.
