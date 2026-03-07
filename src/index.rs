use dashmap::DashSet;
use rayon::prelude::*;
use rust_asm::class_reader::ClassReader;
use rust_asm::class_reader::{AttributeInfo, ElementValue};
use rust_asm::constant_pool::CpInfo;
use rustc_hash::{FxHashMap, FxHashSet};
use std::io::Read;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use serde::{Deserialize, Serialize};
use zip::ZipArchive;

use crate::jvm::descriptor::consume_one_descriptor_type;
use crate::semantic::types::{SymbolProvider, parse_return_type_from_descriptor};

pub mod bucket;
pub mod cache;
pub mod codebase;
pub mod jdk;
pub mod module_graph;
pub mod module_index;
pub mod scope;
pub mod source;
pub mod view;
pub mod workspace_index;

pub use bucket::BucketIndex;
pub use module_graph::ModuleGraph;
pub use module_index::{ClasspathIndex, ModuleIndex, ModuleQueryCache};
pub use scope::{ClasspathId, IndexScope, ModuleId};
pub use view::IndexView;
pub use workspace_index::WorkspaceIndex;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClassMetadata {
    pub package: Option<Arc<str>>,
    pub name: Arc<str>,
    pub internal_name: Arc<str>,
    pub super_name: Option<Arc<str>>,
    pub interfaces: Vec<Arc<str>>,
    /// Class-level annotations
    pub annotations: Vec<AnnotationSummary>,
    pub methods: Vec<MethodSummary>,
    pub fields: Vec<FieldSummary>,
    pub access_flags: u16,
    pub generic_signature: Option<Arc<str>>,
    pub inner_class_of: Option<Arc<str>>,
    pub origin: ClassOrigin,
}

impl ClassMetadata {
    const TARGET_INTERNAL: &'static str = "java/lang/annotation/Target";
    const RETENTION_INTERNAL: &'static str = "java/lang/annotation/Retention";

    /// Get the fully qualified name of the source code that conforms to Java syntax
    pub fn source_name(&self) -> String {
        let mut out = String::new();
        if let Some(ref pkg) = self.package {
            out.push_str(&pkg.replace('/', "."));
            out.push('.');
        }

        if self.inner_class_of.is_some() {
            out.push_str(&self.name.replace('$', "."));
        } else {
            out.push_str(&self.name);
        }
        out
    }

    /// Returns None if no @Target (applicable everywhere).
    /// Returns Some([]) if @Target(value={}) which is practically unusable.
    pub fn annotation_targets(&self) -> Option<Vec<Arc<str>>> {
        let target_ann = self
            .annotations
            .iter()
            .find(|a| a.internal_name.as_ref() == Self::TARGET_INTERNAL)?;

        let value = target_ann.elements.get("value")?;

        let names = match value {
            AnnotationValue::Enum { const_name, .. } => {
                vec![Arc::clone(const_name)]
            }
            AnnotationValue::Array(items) => items
                .iter()
                .filter_map(|item| {
                    if let AnnotationValue::Enum { const_name, .. } = item {
                        Some(Arc::clone(const_name))
                    } else {
                        None
                    }
                })
                .collect(),
            _ => return None,
        };

        Some(names)
    }

    /// "SOURCE", "CLASS", or "RUNTIME". None if no @Retention (defaults to CLASS per JLS).
    pub fn annotation_retention(&self) -> Option<&str> {
        let ann = self
            .annotations
            .iter()
            .find(|a| a.internal_name.as_ref() == Self::RETENTION_INTERNAL)?;

        match ann.elements.get("value")? {
            AnnotationValue::Enum { const_name, .. } => Some(const_name.as_ref()),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AnnotationSummary {
    /// JVM internal name, e.g. "java/lang/Deprecated"
    pub internal_name: Arc<str>,
    /// true = RuntimeVisible*, false = RuntimeInvisible*
    pub runtime_visible: bool,
    pub elements: FxHashMap<Arc<str>, AnnotationValue>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum AnnotationValue {
    Byte(i8),
    Char(u16),
    Double(f64),
    Float(f32),
    Int(i32),
    Long(i64),
    Short(i16),
    Boolean(bool),
    String(Arc<str>),
    Enum {
        type_name: Arc<str>,
        const_name: Arc<str>,
    },
    Class(Arc<str>),
    Nested(Box<AnnotationSummary>),
    Array(Vec<AnnotationValue>),
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum ClassOrigin {
    Jar(Arc<str>),
    SourceFile(Arc<str>),
    ZipSource {
        zip_path: Arc<str>,
        entry_name: Arc<str>,
    },
    Unknown,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct MethodParams {
    pub items: Vec<MethodParam>,
}

impl MethodParams {
    pub fn empty() -> Self {
        Self { items: vec![] }
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 仅由 method descriptor 构建参数列表（name 默认空）
    pub fn from_method_descriptor(method_desc: &str) -> Self {
        let inner = match method_desc.find('(').zip(method_desc.find(')')) {
            Some((l, r)) => &method_desc[l + 1..r],
            None => return Self::empty(),
        };

        let mut items = Vec::new();
        let mut s = inner;
        while !s.is_empty() {
            let (ty, rest) = consume_one_descriptor_type(s);
            if ty.is_empty() {
                break;
            }
            items.push(MethodParam {
                descriptor: Arc::from(ty),
                name: Arc::from(""),
                annotations: Vec::new(),
            });
            s = rest;
        }

        MethodParams { items }
    }

    /// 由 method descriptor + 参数名构建（名字不够会补空）
    pub fn from_descriptor_and_names(method_desc: &str, names: &[Arc<str>]) -> Self {
        let mut out = Self::from_method_descriptor(method_desc);
        for (i, p) in out.items.iter_mut().enumerate() {
            if let Some(n) = names.get(i) {
                p.name = n.clone();
            }
        }
        out
    }

    pub fn param_names(&self) -> Vec<Arc<str>> {
        self.items.iter().map(|i| i.name.clone()).collect()
    }

    pub fn expand(&mut self, other: &MethodParams) {
        if self.items.len() != other.items.len() {
            return;
        }
        for (a, b) in self.items.iter_mut().zip(other.items.iter()) {
            if param_descriptor_compatible_for_name_carryover(&a.descriptor, &b.descriptor)
                && !b.name.is_empty()
            {
                a.name = b.name.clone();
                a.annotations = b.annotations.clone();
            }
        }
    }
}

fn param_descriptor_compatible_for_name_carryover(bytecode_desc: &str, source_desc: &str) -> bool {
    if bytecode_desc == source_desc {
        return true;
    }
    if bytecode_desc.starts_with('[') && source_desc.starts_with('[') {
        return param_descriptor_compatible_for_name_carryover(
            &bytecode_desc[1..],
            &source_desc[1..],
        );
    }
    is_erased_object_descriptor(bytecode_desc) && is_type_variable_descriptor(source_desc)
}

fn is_erased_object_descriptor(desc: &str) -> bool {
    desc == "Ljava/lang/Object;"
}

fn is_type_variable_descriptor(desc: &str) -> bool {
    // JVM generic signatures use Tname;, while our source parser may emit Lname; for type vars.
    if desc.starts_with('T') && desc.ends_with(';') {
        return true;
    }
    if !(desc.starts_with('L') && desc.ends_with(';')) {
        return false;
    }
    let inner = &desc[1..desc.len() - 1];
    if inner.contains('/') || inner.contains('.') || inner.contains('$') || inner.is_empty() {
        return false;
    }
    // Keep rule narrow: treat short all-uppercase identifiers (E, T, K, V, etc.) as type vars.
    inner.len() <= 3 && inner.chars().all(|c| c.is_ascii_uppercase())
}

impl<const N: usize> From<[(&str, &str); N]> for MethodParams {
    fn from(items: [(&str, &str); N]) -> Self {
        MethodParams {
            items: items
                .into_iter()
                .map(|(desc, name)| MethodParam {
                    descriptor: Arc::from(desc),
                    name: Arc::from(name),
                    annotations: Vec::new(),
                })
                .collect(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MethodParam {
    pub descriptor: Arc<str>,
    pub name: Arc<str>,
    pub annotations: Vec<AnnotationSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MethodSummary {
    pub name: Arc<str>,
    pub params: MethodParams,
    /// Method-level annotations
    pub annotations: Vec<AnnotationSummary>,
    pub access_flags: u16,
    pub is_synthetic: bool,
    pub generic_signature: Option<Arc<str>>,
    /// Method return type (Jvm internal name), None if the return type is void (V)
    pub return_type: Option<Arc<str>>,
}

impl MethodSummary {
    pub fn desc(&self) -> Arc<str> {
        let cap: usize = 2
            + self
                .params
                .items
                .iter()
                .map(|p| p.descriptor.len())
                .sum::<usize>()
            + self.return_type.as_ref().map(|r| r.len()).unwrap_or(1);

        let mut out = String::with_capacity(cap);

        out.push('(');
        for p in &self.params.items {
            out.push_str(&p.descriptor);
        }
        out.push(')');
        out.push_str(self.return_type.as_deref().unwrap_or("V"));

        Arc::from(out)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldSummary {
    pub name: Arc<str>,
    pub descriptor: Arc<str>,
    pub access_flags: u16,
    /// Field-level annotations
    pub annotations: Vec<AnnotationSummary>,
    pub is_synthetic: bool,
    pub generic_signature: Option<Arc<str>>,
}

fn collect_decl_annotations(attrs: &[AttributeInfo], cp: &[CpInfo]) -> Vec<AnnotationSummary> {
    let mut out = Vec::new();
    for a in attrs {
        match a {
            AttributeInfo::RuntimeVisibleAnnotations { annotations } => {
                for ann in annotations {
                    if let Some(s) = parse_annotation(ann, cp, true) {
                        out.push(s);
                    }
                }
            }
            AttributeInfo::RuntimeInvisibleAnnotations { annotations } => {
                for ann in annotations {
                    if let Some(s) = parse_annotation(ann, cp, false) {
                        out.push(s);
                    }
                }
            }
            _ => {}
        }
    }
    out
}

fn parse_annotation(
    ann: &rust_asm::class_reader::Annotation,
    cp: &[CpInfo],
    visible: bool,
) -> Option<AnnotationSummary> {
    let internal_name =
        cp_utf8_desc_to_internal(cp, ann.type_descriptor_index).map(|s| intern_str(&s))?;

    let mut elements: FxHashMap<Arc<str>, AnnotationValue> = FxHashMap::default();
    for pair in &ann.element_value_pairs {
        let key = match cp.get(pair.element_name_index as usize) {
            Some(CpInfo::Utf8(s)) => Arc::from(s.as_str()),
            _ => continue,
        };
        let value = parse_element_value(&pair.value, cp);
        elements.insert(key, value);
    }

    Some(AnnotationSummary {
        internal_name,
        runtime_visible: visible,
        elements,
    })
}

fn parse_element_value(
    ev: &rust_asm::class_reader::ElementValue,
    cp: &[CpInfo],
) -> AnnotationValue {
    match ev {
        ElementValue::ConstValueIndex {
            tag,
            const_value_index,
        } => parse_const_value(*tag, *const_value_index, cp),
        ElementValue::EnumConstValue {
            type_name_index,
            const_name_index,
        } => {
            let type_name = cp_utf8(cp, *type_name_index).unwrap_or("?");
            let const_name = cp_utf8(cp, *const_name_index).unwrap_or("?");
            AnnotationValue::Enum {
                type_name: Arc::from(type_name),
                const_name: Arc::from(const_name),
            }
        }
        ElementValue::ClassInfoIndex { class_info_index } => {
            let s = cp_utf8(cp, *class_info_index).unwrap_or("?");
            AnnotationValue::Class(Arc::from(s))
        }
        ElementValue::AnnotationValue(inner) => match parse_annotation(inner, cp, true) {
            Some(s) => AnnotationValue::Nested(Box::new(s)),
            None => AnnotationValue::Unknown,
        },
        ElementValue::ArrayValue(items) => {
            AnnotationValue::Array(items.iter().map(|i| parse_element_value(i, cp)).collect())
        }
    }
}

fn parse_const_value(tag: u8, idx: u16, cp: &[CpInfo]) -> AnnotationValue {
    match tag {
        b'B' => cp_int(cp, idx)
            .map(|v| AnnotationValue::Byte(v as i8))
            .unwrap_or(AnnotationValue::Unknown),
        b'C' => cp_int(cp, idx)
            .map(|v| AnnotationValue::Char(v as u16))
            .unwrap_or(AnnotationValue::Unknown),
        b'D' => cp_double(cp, idx)
            .map(AnnotationValue::Double)
            .unwrap_or(AnnotationValue::Unknown),
        b'F' => cp_float(cp, idx)
            .map(AnnotationValue::Float)
            .unwrap_or(AnnotationValue::Unknown),
        b'I' => cp_int(cp, idx)
            .map(AnnotationValue::Int)
            .unwrap_or(AnnotationValue::Unknown),
        b'J' => cp_long(cp, idx)
            .map(AnnotationValue::Long)
            .unwrap_or(AnnotationValue::Unknown),
        b'S' => cp_int(cp, idx)
            .map(|v| AnnotationValue::Short(v as i16))
            .unwrap_or(AnnotationValue::Unknown),
        b'Z' => cp_int(cp, idx)
            .map(|v| AnnotationValue::Boolean(v != 0))
            .unwrap_or(AnnotationValue::Unknown),
        b's' => cp_utf8(cp, idx)
            .map(|s| AnnotationValue::String(Arc::from(s)))
            .unwrap_or(AnnotationValue::Unknown),
        _ => AnnotationValue::Unknown,
    }
}

fn cp_utf8(cp: &[CpInfo], idx: u16) -> Option<&str> {
    match cp.get(idx as usize)? {
        CpInfo::Utf8(s) => Some(s.as_str()),
        _ => None,
    }
}

fn cp_int(cp: &[CpInfo], idx: u16) -> Option<i32> {
    match cp.get(idx as usize)? {
        CpInfo::Integer(v) => Some(*v),
        _ => None,
    }
}

fn cp_long(cp: &[CpInfo], idx: u16) -> Option<i64> {
    match cp.get(idx as usize)? {
        CpInfo::Long(v) => Some(*v),
        _ => None,
    }
}

fn cp_float(cp: &[CpInfo], idx: u16) -> Option<f32> {
    match cp.get(idx as usize)? {
        CpInfo::Float(v) => Some(*v),
        _ => None,
    }
}

fn cp_double(cp: &[CpInfo], idx: u16) -> Option<f64> {
    match cp.get(idx as usize)? {
        CpInfo::Double(v) => Some(*v),
        _ => None,
    }
}

fn cp_utf8_desc_to_internal(cp: &[CpInfo], idx: u16) -> Option<String> {
    let s = match cp.get(idx as usize)? {
        CpInfo::Utf8(u) => u.as_str(),
        _ => return None,
    };
    // Expect "Lpkg/Name;" or "[L..;"
    let s = s.trim();
    let s = s.strip_prefix('L')?.strip_suffix(';')?;
    Some(s.to_string())
}

fn build_method_params_from_attrs(
    descriptor: &str,
    attrs: &[AttributeInfo],
    cp: &[CpInfo],
) -> MethodParams {
    let mut params = MethodParams::from_method_descriptor(descriptor);
    let param_count = params.items.len();
    let mut names: Vec<Arc<str>> = vec![Arc::from(""); param_count];

    // MethodParameters names
    if let Some(AttributeInfo::MethodParameters { parameters }) = attrs
        .iter()
        .find(|a| matches!(a, AttributeInfo::MethodParameters { .. }))
    {
        for (i, p) in parameters.iter().enumerate().take(param_count) {
            if p.name_index != 0
                && let Some(CpInfo::Utf8(s)) = cp.get(p.name_index as usize)
            {
                names[i] = Arc::from(s.as_str());
            }
        }
    }

    // Parameter annotations
    let mut annos: Vec<Vec<AnnotationSummary>> = vec![Vec::new(); param_count];
    for a in attrs {
        match a {
            AttributeInfo::RuntimeVisibleParameterAnnotations { parameters } => {
                merge_param_annos(&mut annos, parameters, cp, true);
            }
            AttributeInfo::RuntimeInvisibleParameterAnnotations { parameters } => {
                merge_param_annos(&mut annos, parameters, cp, false);
            }
            _ => {}
        }
    }

    for (i, p) in params.items.iter_mut().enumerate() {
        p.annotations = annos.get(i).cloned().unwrap_or_default();
    }

    params
}

fn merge_param_annos(
    out: &mut [Vec<AnnotationSummary>],
    parameters: &rust_asm::class_reader::ParameterAnnotations,
    cp: &[CpInfo],
    visible: bool,
) {
    for (i, anns) in parameters.parameters.iter().enumerate() {
        if i >= out.len() {
            break;
        }
        for ann in anns {
            if let Some(s) = parse_annotation(ann, cp, visible) {
                out[i].push(s);
            }
        }
    }
}

pub fn index_jar<P: AsRef<Path>>(path: P) -> anyhow::Result<Vec<ClassMetadata>> {
    let path = path.as_ref();
    // Try to load from cache
    if let Some(cached) = cache::load_cached(path) {
        return Ok(cached);
    }

    let classes = index_jar_uncached(path)?;

    // save cache
    let classes_clone = classes.clone();
    let path_buf = path.to_path_buf();
    std::thread::spawn(move || {
        cache::save_cache(&path_buf, &classes_clone);
    });

    Ok(classes)
}

fn index_jar_uncached(path: &Path) -> anyhow::Result<Vec<ClassMetadata>> {
    let jar_str = Arc::from(path.to_string_lossy().as_ref());
    let file = std::fs::File::open(path)?;
    let mut archive = ZipArchive::new(std::io::BufReader::new(file))?;

    // 收集所有文件内容（class、java、kt）
    let mut class_files: Vec<(String, Vec<u8>)> = Vec::new();
    let mut source_files: Vec<(String, Vec<u8>)> = Vec::new();

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let name = entry.name().to_string();
        let mut buf = Vec::new();
        entry.read_to_end(&mut buf)?;

        if name.ends_with(".class") {
            class_files.push((name, buf));
        } else if name.ends_with(".java") || name.ends_with(".kt") {
            source_files.push((name, buf));
        }
    }

    // 解析字节码（跳过已有源文件的类）
    let mut bytecode_results: Vec<ClassMetadata> = class_files
        .into_par_iter()
        .filter_map(|(name, bytes)| parse_class_data_bytes(&name, &bytes, Arc::clone(&jar_str)))
        .collect();

    let source_results = parse_source_files_parallel(&source_files, &jar_str);
    merge_source_into_bytecode(&mut bytecode_results, source_results);

    Ok(bytecode_results)
}

/// Parse class bytes with an explicit origin.
/// `file_name` is used only for debugging; the actual class name comes from the bytecode.
pub(crate) fn parse_class_data_bytes(
    _file_name: &str,
    bytes: &[u8],
    origin: Arc<str>,
) -> Option<ClassMetadata> {
    parse_class_data_with_origin(bytes, ClassOrigin::Jar(origin))
}

/// Internal: parse class bytes with arbitrary origin.
fn parse_class_data_with_origin(bytes: &[u8], origin: ClassOrigin) -> Option<ClassMetadata> {
    let cr = ClassReader::new(bytes);
    let cn = cr.to_class_node().ok()?;

    let internal_name: Arc<str> = Arc::from(cn.name.as_str());
    let rsp: Vec<_> = cn.name.rsplitn(2, '/').collect();
    let class_name = *rsp.first()?;
    let package = rsp.get(1).copied();

    let generic_signature = cn.attributes.iter().find_map(|a| {
        if let AttributeInfo::Signature { signature_index } = a {
            cn.constant_pool
                .get(*signature_index as usize)
                .and_then(|cp| {
                    if let CpInfo::Utf8(s) = cp {
                        Some(Arc::from(s.as_str()))
                    } else {
                        None
                    }
                })
        } else {
            None
        }
    });

    let methods: Vec<MethodSummary> = cn
        .methods
        .iter()
        .map(|md| {
            let is_synthetic = md
                .attributes
                .iter()
                .any(|a| matches!(a, AttributeInfo::Synthetic));
            let generic_signature = md.attributes.iter().find_map(|a| {
                if let AttributeInfo::Signature { signature_index } = a {
                    cn.constant_pool
                        .get(*signature_index as usize)
                        .and_then(|cp| {
                            if let CpInfo::Utf8(s) = cp {
                                Some(Arc::from(s.as_str()))
                            } else {
                                None
                            }
                        })
                } else {
                    None
                }
            });
            let return_type = parse_return_type_from_descriptor(&md.descriptor);
            let params =
                build_method_params_from_attrs(&md.descriptor, &md.attributes, &cn.constant_pool);
            MethodSummary {
                name: Arc::from(md.name.as_str()),
                access_flags: md.access_flags,
                params,
                annotations: collect_decl_annotations(&md.attributes, &cn.constant_pool),
                is_synthetic,
                generic_signature,
                return_type,
            }
        })
        .collect();

    let traced_add: Vec<String> = methods
        .iter()
        .filter(|m| m.name.as_ref() == "add")
        .map(|m| {
            format!(
                "desc={} gs={:?} ret={:?} params={:?} names={:?}",
                m.desc(),
                m.generic_signature,
                m.return_type,
                m.params
                    .items
                    .iter()
                    .map(|p| p.descriptor.as_ref())
                    .collect::<Vec<_>>(),
                m.params
                    .items
                    .iter()
                    .map(|p| p.name.as_ref())
                    .collect::<Vec<_>>()
            )
        })
        .collect();
    if !traced_add.is_empty() && internal_name.contains("ArrayList") {
        tracing::debug!(
            class_internal = %internal_name,
            class_generic_signature = ?generic_signature,
            origin = ?origin,
            add_overloads = ?traced_add,
            "index::parse_class_data_with_origin: bytecode class parsed"
        );
    }

    let fields = cn
        .fields
        .iter()
        .map(|fd| {
            let is_synthetic = fd
                .attributes
                .iter()
                .any(|a| matches!(a, AttributeInfo::Synthetic));
            let generic_signature = fd.attributes.iter().find_map(|a| {
                if let AttributeInfo::Signature { signature_index } = a {
                    cn.constant_pool
                        .get(*signature_index as usize)
                        .and_then(|cp| {
                            if let CpInfo::Utf8(s) = cp {
                                Some(Arc::from(s.as_str()))
                            } else {
                                None
                            }
                        })
                } else {
                    None
                }
            });

            FieldSummary {
                name: Arc::from(fd.name.as_str()),
                descriptor: Arc::from(fd.descriptor.as_str()),
                access_flags: fd.access_flags,
                annotations: collect_decl_annotations(&fd.attributes, &cn.constant_pool),
                is_synthetic,
                generic_signature,
            }
        })
        .collect();

    let inner_class_of = if !cn.outer_class.is_empty() {
        let outer_simple = cn.outer_class.rsplit('/').next().unwrap_or(&cn.outer_class);
        Some(Arc::from(outer_simple))
    } else {
        class_name
            .find('$')
            .map(|pos| Arc::from(&class_name[..pos]))
    };

    Some(ClassMetadata {
        package: package.map(Arc::from),
        name: Arc::from(class_name),
        internal_name,
        super_name: cn.super_name.map(|str| intern_str(&str)),
        interfaces: cn
            .interfaces
            .iter()
            .map(|name| intern_str(name.as_str()))
            .collect(),
        annotations: collect_decl_annotations(&cn.attributes, &cn.constant_pool),
        methods,
        fields,
        access_flags: cn.access_flags,
        generic_signature,
        inner_class_of,
        origin,
    })
}

/// 将源码中的参数名称合并到字节码索引中，并使用简单的类型名称比对解决重载冲突
pub fn merge_source_into_bytecode(bytecode: &mut [ClassMetadata], source: Vec<ClassMetadata>) {
    let mut source_map: rustc_hash::FxHashMap<Arc<str>, ClassMetadata> = source
        .into_iter()
        .map(|c| (c.internal_name.clone(), c))
        .collect();

    for b_class in bytecode.iter_mut() {
        // 如果在源码中找到了对应的类
        if let Some(s_class) = source_map.remove(&b_class.internal_name) {
            b_class.origin = s_class.origin; // 提升来源标识为源码(方便跳转)
            if b_class.internal_name.contains("ArrayList") {
                tracing::debug!(
                    class_internal = %b_class.internal_name,
                    new_origin = ?b_class.origin,
                    source_add = ?s_class
                        .methods
                        .iter()
                        .filter(|m| m.name.as_ref() == "add")
                        .map(|m| format!(
                            "desc={} gs={:?} ret={:?} params={:?} names={:?}",
                            m.desc(),
                            m.generic_signature,
                            m.return_type,
                            m.params.items.iter().map(|p| p.descriptor.as_ref()).collect::<Vec<_>>(),
                            m.params.items.iter().map(|p| p.name.as_ref()).collect::<Vec<_>>(),
                        ))
                        .collect::<Vec<_>>(),
                    bytecode_add_before_merge = ?b_class
                        .methods
                        .iter()
                        .filter(|m| m.name.as_ref() == "add")
                        .map(|m| format!(
                            "desc={} gs={:?} ret={:?} params={:?} names={:?}",
                            m.desc(),
                            m.generic_signature,
                            m.return_type,
                            m.params.items.iter().map(|p| p.descriptor.as_ref()).collect::<Vec<_>>(),
                            m.params.items.iter().map(|p| p.name.as_ref()).collect::<Vec<_>>(),
                        ))
                        .collect::<Vec<_>>(),
                    "index::merge_source_into_bytecode: before param-name merge"
                );
            }

            for b_method in b_class.methods.iter_mut() {
                let b_param_count = b_method.params.len();
                let b_is_add = b_method.name.as_ref() == "add";
                let b_desc_now = b_method.desc();

                // 找同名、同参数数量的候选
                let candidates: Vec<&MethodSummary> = s_class
                    .methods
                    .iter()
                    .filter(|m| m.name == b_method.name && m.params.len() == b_param_count)
                    .collect();

                if b_is_add {
                    tracing::debug!(
                        class_internal = %b_class.internal_name,
                        new_origin = ?b_class.origin,
                        method_name = %b_method.name,
                        bytecode_desc = %b_desc_now,
                        bytecode_generic_signature = ?b_method.generic_signature,
                        bytecode_param_names_before = ?b_method.params.items.iter().map(|p| p.name.as_ref()).collect::<Vec<_>>(),
                        bytecode_simple_types = ?extract_simple_types(&b_desc_now),
                        source_candidates = ?candidates
                            .iter()
                            .map(|m| format!(
                                "desc={} gs={:?} names={:?} simple={:?}",
                                m.desc(),
                                m.generic_signature,
                                m.params.items.iter().map(|p| p.name.as_ref()).collect::<Vec<_>>(),
                                extract_simple_types(&m.desc())
                            ))
                            .collect::<Vec<_>>(),
                        "index::merge_source_into_bytecode: add matching candidates"
                    );
                }

                if candidates.len() == 1 {
                    b_method.params.expand(&candidates[0].params);
                    if b_is_add {
                        tracing::debug!(
                            class_internal = %b_class.internal_name,
                            method_name = %b_method.name,
                            match_strategy = "single-candidate",
                            matched_source_desc = %candidates[0].desc(),
                            merged_param_names_after = ?b_method.params.items.iter().map(|p| p.name.as_ref()).collect::<Vec<_>>(),
                            "index::merge_source_into_bytecode: add merge result"
                        );
                    }
                } else if candidates.len() > 1 {
                    // 发生重载冲突时，使用参数的简单名称进行模糊匹配对齐 (例如 String 匹配 java/lang/String)
                    let b_simple = extract_simple_types(&b_method.desc());
                    if let Some(best) = candidates
                        .iter()
                        .find(|m| extract_simple_types(&m.desc()) == b_simple)
                    {
                        b_method.params.expand(&best.params);
                        if b_is_add {
                            tracing::debug!(
                                class_internal = %b_class.internal_name,
                                method_name = %b_method.name,
                                match_strategy = "simple-types-exact",
                                matched_source_desc = %best.desc(),
                                matched_source_simple = ?extract_simple_types(&best.desc()),
                                bytecode_simple = ?b_simple,
                                merged_param_names_after = ?b_method.params.items.iter().map(|p| p.name.as_ref()).collect::<Vec<_>>(),
                                "index::merge_source_into_bytecode: add merge result"
                            );
                        }
                    } else {
                        b_method.params.expand(&candidates[0].params); // 保底
                        if b_is_add {
                            tracing::debug!(
                                class_internal = %b_class.internal_name,
                                method_name = %b_method.name,
                                match_strategy = "fallback-first-candidate",
                                matched_source_desc = %candidates[0].desc(),
                                merged_param_names_after = ?b_method.params.items.iter().map(|p| p.name.as_ref()).collect::<Vec<_>>(),
                                "index::merge_source_into_bytecode: add merge result"
                            );
                        }
                    }
                }

                if b_class.internal_name.contains("ArrayList") && b_method.name.as_ref() == "add" {
                    tracing::debug!(
                        class_internal = %b_class.internal_name,
                        method_name = %b_method.name,
                        method_desc = %b_method.desc(),
                        method_generic_signature = ?b_method.generic_signature,
                        method_return_type = ?b_method.return_type,
                        merged_param_descriptors = ?b_method
                            .params
                            .items
                            .iter()
                            .map(|p| p.descriptor.as_ref())
                            .collect::<Vec<_>>(),
                        merged_param_names = ?b_method
                            .params
                            .items
                            .iter()
                            .map(|p| p.name.as_ref())
                            .collect::<Vec<_>>(),
                        "index::merge_source_into_bytecode: add overload after param merge"
                    );
                }
            }
        }
    }
}

fn extract_simple_types(desc: &str) -> Vec<String> {
    let inner = match desc.find('(').zip(desc.find(')')) {
        Some((l, r)) => &desc[l + 1..r],
        None => return vec![],
    };
    let mut types = vec![];
    let mut s = inner;
    while !s.is_empty() {
        let (ty, rest) = consume_one_descriptor_type(s);
        if ty.is_empty() {
            break;
        }

        let mut ty_str = ty;
        let mut prefix = String::new();
        while ty_str.starts_with('[') {
            prefix.push('[');
            ty_str = &ty_str[1..];
        }

        let simple = if ty_str.starts_with('L') && ty_str.ends_with(';') {
            let internal = &ty_str[1..ty_str.len() - 1];
            internal.rsplit('/').next().unwrap_or(internal)
        } else {
            ty_str
        };

        types.push(format!("{}{}", prefix, simple));
        s = rest;
    }
    types
}

pub(crate) fn intern_str(s: &str) -> Arc<str> {
    static POOL: OnceLock<DashSet<Arc<str>>> = OnceLock::new();
    let pool = POOL.get_or_init(DashSet::new);

    if let Some(arc) = pool.get(s) {
        return Arc::clone(&arc);
    }

    let arc: Arc<str> = Arc::from(s);
    pool.insert(Arc::clone(&arc));
    arc
}

/// Lightweight index snapshot: a set of all JVM internal names.
/// `Send + Sync + 'static` — safe to clone into `spawn_blocking` closures.
#[derive(Clone)]
pub struct NameTable(FxHashSet<Arc<str>>);

impl NameTable {
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn exists(&self, internal_name: &str) -> bool {
        self.0.contains(internal_name)
    }

    /// Build directly from a class slice — no GlobalIndex needed.
    /// Used by JDK indexer to resolve source types against already-parsed bytecode.
    pub fn from_classes(classes: &[ClassMetadata]) -> Arc<Self> {
        Arc::new(NameTable(
            classes
                .iter()
                .map(|c| Arc::clone(&c.internal_name))
                .collect(),
        ))
    }

    pub fn extend_with(&self, additional_names: Vec<Arc<str>>) -> Arc<Self> {
        let mut set = self.0.clone();
        set.extend(additional_names);
        Arc::new(NameTable(set))
    }

    pub fn from_names(names: Vec<Arc<str>>) -> Arc<Self> {
        Arc::new(NameTable(names.into_iter().collect()))
    }
}

pub struct GlobalIndex {
    bucket: BucketIndex,
}

impl GlobalIndex {
    pub fn new() -> Self {
        Self {
            bucket: BucketIndex::new(),
        }
    }

    pub fn add_classes(&mut self, classes: Vec<ClassMetadata>) {
        self.bucket.add_classes(classes);
    }

    /// Parse multiple JARs in parallel and write them to the index in batches
    /// Faster than calling add_classes one by one because it only rebuilds the class once. (fuzzy)
    pub fn add_classes_bulk(&mut self, batch: Vec<Vec<ClassMetadata>>) {
        let flat: Vec<ClassMetadata> = batch.into_iter().flatten().collect();
        self.add_classes(flat);
    }

    pub fn remove_by_origin(&mut self, origin: &ClassOrigin) {
        self.bucket.remove_by_origin(origin);
    }

    pub fn update_source(&mut self, origin: ClassOrigin, classes: Vec<ClassMetadata>) {
        self.bucket.update_source(origin, classes);
    }

    pub fn get_class(&self, internal_name: &str) -> Option<Arc<ClassMetadata>> {
        self.bucket.get_class(internal_name)
    }

    /// Attempt to retrieve the source code format name; return None if the index does not exist.
    pub fn get_source_type_name(&self, internal: &str) -> Option<String> {
        self.bucket.get_source_type_name(internal)
    }

    pub fn get_classes_by_simple_name(&self, simple_name: &str) -> Vec<Arc<ClassMetadata>> {
        self.bucket.get_classes_by_simple_name(simple_name)
    }

    // 返回切片，生命周期与 &self 绑定
    pub fn classes_in_package(&self, pkg: &str) -> Vec<Arc<ClassMetadata>> {
        self.bucket.classes_in_package(pkg)
    }

    pub fn has_package(&self, pkg: &str) -> bool {
        self.bucket.has_package(pkg)
    }

    pub fn has_classes_in_package(&self, pkg: &str) -> bool {
        self.bucket.has_classes_in_package(pkg)
    }

    pub fn resolve_imports(&self, imports: &[Arc<str>]) -> Vec<Arc<ClassMetadata>> {
        self.bucket.resolve_imports(imports)
    }

    /// Collect all methods and fields visible on `class_internal`,
    /// walking the inheritance chain (super_name + interfaces).
    /// Stops at classes not present in the index (e.g. java/lang/Object if not indexed).
    /// Returns (methods, fields) deduplicated by name — subclass members shadow superclass.
    pub fn collect_inherited_members(
        &self,
        class_internal: &str,
    ) -> (Vec<Arc<MethodSummary>>, Vec<Arc<FieldSummary>>) {
        self.bucket.collect_inherited_members(class_internal)
    }

    /// Return all classes in the MRO (Method Resolution Order) of `class_internal`,
    /// starting from the class itself, walking super_name then interfaces.
    /// Classes not in the index are silently skipped.
    pub fn mro(&self, class_internal: &str) -> Vec<Arc<ClassMetadata>> {
        self.bucket.mro(class_internal)
    }

    pub fn fuzzy_autocomplete(&mut self, query: &str, limit: usize) -> Vec<Arc<str>> {
        self.bucket.fuzzy_autocomplete(query, limit)
    }

    pub fn fuzzy_search_classes(&mut self, query: &str, limit: usize) -> Vec<Arc<ClassMetadata>> {
        self.bucket.fuzzy_search_classes(query, limit)
    }

    pub fn exact_match_keys(&self) -> Vec<Arc<str>> {
        self.bucket.exact_match_keys()
    }

    pub fn class_count(&self) -> usize {
        self.bucket.class_count()
    }

    /// Iterate through all indexed classes (used for scenarios requiring a full scan, such as import completion).
    pub fn iter_all_classes(&self) -> Vec<Arc<ClassMetadata>> {
        self.bucket.iter_all_classes()
    }

    /// Build a lightweight snapshot for use across `spawn_blocking` boundaries.
    pub fn build_name_table(&self) -> Arc<NameTable> {
        self.bucket.build_name_table()
    }
}

impl Default for GlobalIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl SymbolProvider for GlobalIndex {
    fn resolve_source_name(&self, internal_name: &str) -> Option<String> {
        self.get_source_type_name(internal_name)
    }
}

fn parse_source_files_parallel(
    files: &[(String, Vec<u8>)],
    jar_origin: &Arc<str>,
) -> Vec<ClassMetadata> {
    files
        .par_iter()
        .flat_map(|(name, bytes)| {
            let content = match std::str::from_utf8(bytes) {
                Ok(s) => s,
                Err(_) => return vec![],
            };
            let lang = if name.ends_with(".kt") {
                "kotlin"
            } else {
                "java"
            };
            let origin = ClassOrigin::Jar(Arc::clone(jar_origin));
            source::parse_source_str(content, lang, origin, None)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use rust_asm::constants::ACC_PUBLIC;

    use super::*;
    use crate::semantic::types::count_params;

    fn make_class(pkg: &str, name: &str, origin: ClassOrigin) -> ClassMetadata {
        let internal = format!("{}/{}", pkg, name);
        ClassMetadata {
            package: Some(Arc::from(pkg)),
            name: Arc::from(name),
            internal_name: Arc::from(internal.as_str()),
            super_name: None,
            interfaces: vec![],
            annotations: vec![],
            methods: vec![],
            fields: vec![],
            access_flags: ACC_PUBLIC,
            inner_class_of: None,
            generic_signature: None,
            origin,
        }
    }

    fn make_method(name: &str, descriptor: &str) -> MethodSummary {
        MethodSummary {
            name: Arc::from(name),
            access_flags: ACC_PUBLIC,
            is_synthetic: false,
            params: MethodParams::from_method_descriptor(descriptor),
            annotations: vec![],
            generic_signature: None,
            return_type: parse_return_type_from_descriptor(descriptor),
        }
    }

    // TODO: field tests

    // fn make_field(name: &str, descriptor: &str) -> FieldSummary {
    //     FieldSummary {
    //         name: Arc::from(name),
    //         descriptor: Arc::from(descriptor),
    //         access_flags: ACC_PUBLIC,
    //         is_synthetic: false,
    //     }
    // }

    #[test]
    fn test_add_and_get_class() {
        let mut idx = GlobalIndex::new();
        idx.add_classes(vec![make_class("com/example", "Foo", ClassOrigin::Unknown)]);
        assert!(idx.get_class("com/example/Foo").is_some());
        assert!(idx.get_class("com/example/Bar").is_none());
    }

    #[test]
    fn test_class_count() {
        let mut idx = GlobalIndex::new();
        assert_eq!(idx.class_count(), 0);
        idx.add_classes(vec![
            make_class("a", "A", ClassOrigin::Unknown),
            make_class("b", "B", ClassOrigin::Unknown),
        ]);
        assert_eq!(idx.class_count(), 2);
    }

    #[test]
    fn test_simple_name_lookup_same_name_different_package() {
        let mut idx = GlobalIndex::new();
        idx.add_classes(vec![
            make_class("com/a", "Utils", ClassOrigin::Unknown),
            make_class("com/b", "Utils", ClassOrigin::Unknown),
        ]);
        let results = idx.get_classes_by_simple_name("Utils");
        assert_eq!(results.len(), 2);
        // 两个包都在
        let pkgs: Vec<_> = results
            .iter()
            .map(|c| c.package.as_deref().unwrap_or(""))
            .collect();
        assert!(pkgs.contains(&"com/a"));
        assert!(pkgs.contains(&"com/b"));
    }

    #[test]
    fn test_simple_name_lookup_missing() {
        let idx = GlobalIndex::new();
        assert!(idx.get_classes_by_simple_name("Missing").is_empty());
    }

    // ── package_index / 通配符 ────────────────────────────────────────────────

    #[test]
    fn test_package_index() {
        let mut idx = GlobalIndex::new();
        idx.add_classes(vec![
            make_class("java/util", "List", ClassOrigin::Unknown),
            make_class("java/util", "Map", ClassOrigin::Unknown),
            make_class("java/io", "File", ClassOrigin::Unknown),
        ]);
        let pkg = idx.classes_in_package("java/util");
        assert_eq!(pkg.len(), 2);
        assert!(pkg.iter().any(|c| c.name.as_ref() == "List"));
        assert!(pkg.iter().any(|c| c.name.as_ref() == "Map"));
        // java/io 不在里面
        assert!(pkg.iter().all(|c| c.name.as_ref() != "File"));
    }

    #[test]
    fn test_package_index_dot_notation() {
        // classes_in_package 应该同时接受 "java.util" 和 "java/util"
        let mut idx = GlobalIndex::new();
        idx.add_classes(vec![make_class("java/util", "List", ClassOrigin::Unknown)]);
        assert_eq!(idx.classes_in_package("java.util").len(), 1);
        assert_eq!(idx.classes_in_package("java/util").len(), 1);
    }

    #[test]
    fn test_wildcard_import_resolve() {
        let mut idx = GlobalIndex::new();
        idx.add_classes(vec![
            make_class("java/util", "List", ClassOrigin::Unknown),
            make_class("java/util", "ArrayList", ClassOrigin::Unknown),
            make_class("java/io", "File", ClassOrigin::Unknown),
        ]);

        // java.util.* → List + ArrayList
        let resolved = idx.resolve_imports(&["java.util.*".into()]);
        assert_eq!(resolved.len(), 2);
        assert!(resolved.iter().any(|c| c.name.as_ref() == "List"));
        assert!(resolved.iter().any(|c| c.name.as_ref() == "ArrayList"));
    }

    #[test]
    fn test_exact_import_resolve() {
        let mut idx = GlobalIndex::new();
        idx.add_classes(vec![make_class("java/io", "File", ClassOrigin::Unknown)]);
        let resolved = idx.resolve_imports(&["java.io.File".into()]);
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].name.as_ref(), "File");
    }

    #[test]
    fn test_mixed_import_resolve() {
        let mut idx = GlobalIndex::new();
        idx.add_classes(vec![
            make_class("java/util", "List", ClassOrigin::Unknown),
            make_class("java/util", "ArrayList", ClassOrigin::Unknown),
            make_class("java/io", "File", ClassOrigin::Unknown),
        ]);
        let imports = vec!["java.util.*".into(), "java.io.File".into()];
        let resolved = idx.resolve_imports(&imports);
        assert_eq!(resolved.len(), 3);
    }

    #[test]
    fn test_unknown_import_returns_empty() {
        let idx = GlobalIndex::new();
        let resolved = idx.resolve_imports(&["com.nonexistent.Foo".into()]);
        assert!(resolved.is_empty());
    }

    // ── remove_by_origin ─────────────────────────────────────────────────────

    #[test]
    fn test_remove_by_origin_removes_exact_match() {
        let origin = ClassOrigin::SourceFile(Arc::from("file:///A.java"));
        let mut idx = GlobalIndex::new();
        idx.add_classes(vec![
            make_class("com/example", "A", origin.clone()),
            make_class("com/example", "B", ClassOrigin::Unknown),
        ]);

        idx.remove_by_origin(&origin);

        assert!(
            idx.get_class("com/example/A").is_none(),
            "A should be removed"
        );
        assert!(idx.get_class("com/example/B").is_some(), "B should remain");
    }

    #[test]
    fn test_remove_by_origin_updates_simple_name_index() {
        let origin = ClassOrigin::SourceFile(Arc::from("file:///A.java"));
        let mut idx = GlobalIndex::new();
        idx.add_classes(vec![
            make_class("com/a", "Util", origin.clone()),
            make_class("com/b", "Util", ClassOrigin::Unknown),
        ]);

        idx.remove_by_origin(&origin);

        // com/a の Util は消えるが com/b は残る
        let results = idx.get_classes_by_simple_name("Util");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].package.as_deref(), Some("com/b"));
    }

    #[test]
    fn test_remove_by_origin_updates_package_index() {
        let origin = ClassOrigin::SourceFile(Arc::from("file:///A.java"));
        let mut idx = GlobalIndex::new();
        idx.add_classes(vec![
            make_class("com/example", "A", origin.clone()),
            make_class("com/example", "B", ClassOrigin::Unknown),
        ]);

        idx.remove_by_origin(&origin);

        let pkg = idx.classes_in_package("com/example");
        assert_eq!(pkg.len(), 1);
        assert_eq!(pkg[0].name.as_ref(), "B");
    }

    #[test]
    fn test_remove_by_origin_nonexistent_is_noop() {
        let mut idx = GlobalIndex::new();
        idx.add_classes(vec![make_class("com/example", "A", ClassOrigin::Unknown)]);
        // 删除不存在的 origin 不应 panic
        let fake_origin = ClassOrigin::SourceFile(Arc::from("file:///nonexistent.java"));
        idx.remove_by_origin(&fake_origin);
        assert_eq!(idx.class_count(), 1);
    }

    // ── update_source ─────────────────────────────────────────────────────────

    #[test]
    fn test_update_source_replaces_old_version() {
        let uri = Arc::from("file:///Service.java");
        let origin = ClassOrigin::SourceFile(Arc::clone(&uri));
        let mut idx = GlobalIndex::new();

        // v1: Service 有 oldMethod
        let mut v1 = make_class("com/example", "Service", origin.clone());
        v1.methods.push(make_method("oldMethod", "()V"));
        idx.add_classes(vec![v1]);

        assert!(
            idx.get_class("com/example/Service")
                .unwrap()
                .methods
                .iter()
                .any(|m| m.name.as_ref() == "oldMethod")
        );

        // v2: Service 有 newMethod
        let mut v2 = make_class("com/example", "Service", origin.clone());
        v2.methods.push(make_method("newMethod", "()V"));
        idx.update_source(origin, vec![v2]);

        let cls = idx.get_class("com/example/Service").unwrap();
        assert!(
            !cls.methods.iter().any(|m| m.name.as_ref() == "oldMethod"),
            "oldMethod should be gone"
        );
        assert!(
            cls.methods.iter().any(|m| m.name.as_ref() == "newMethod"),
            "newMethod should be present"
        );
    }

    #[test]
    fn test_update_source_can_add_new_class() {
        let uri = Arc::from("file:///Foo.java");
        let origin = ClassOrigin::SourceFile(Arc::clone(&uri));
        let mut idx = GlobalIndex::new();

        // 初始空
        assert_eq!(idx.class_count(), 0);

        // 第一次 update_source（相当于首次打开文件）
        idx.update_source(
            origin,
            vec![make_class(
                "com/example",
                "Foo",
                ClassOrigin::SourceFile(Arc::clone(&uri)),
            )],
        );
        assert_eq!(idx.class_count(), 1);
        assert!(idx.get_class("com/example/Foo").is_some());
    }

    #[test]
    fn test_update_source_can_remove_class_if_deleted_from_file() {
        let uri = Arc::from("file:///Multi.java");
        let origin = ClassOrigin::SourceFile(Arc::clone(&uri));
        let mut idx = GlobalIndex::new();

        // v1: 文件里有 A 和 B
        idx.add_classes(vec![
            make_class("com/example", "A", origin.clone()),
            make_class("com/example", "B", origin.clone()),
        ]);
        assert_eq!(idx.class_count(), 2);

        // v2: 文件里只剩 A（用户删除了 B）
        idx.update_source(origin.clone(), vec![make_class("com/example", "A", origin)]);
        assert_eq!(idx.class_count(), 1);
        assert!(idx.get_class("com/example/A").is_some());
        assert!(idx.get_class("com/example/B").is_none());
    }

    #[test]
    fn test_parse_return_type_object() {
        assert_eq!(
            parse_return_type_from_descriptor("()Ljava/util/List;").as_deref(),
            Some("Ljava/util/List;")
        );
    }

    #[test]
    fn test_parse_return_type_void() {
        assert_eq!(parse_return_type_from_descriptor("()V"), None);
    }

    #[test]
    fn test_parse_return_type_primitive() {
        assert_eq!(
            parse_return_type_from_descriptor("()I").as_deref(),
            Some("I")
        );
        assert_eq!(
            parse_return_type_from_descriptor("()Z").as_deref(),
            Some("Z")
        );
    }

    #[test]
    fn test_count_params_various() {
        assert_eq!(count_params("()V"), 0);
        assert_eq!(count_params("(I)V"), 1);
        assert_eq!(count_params("(IZ)V"), 2);
        assert_eq!(count_params("(ILjava/lang/String;[B)V"), 3);
        assert_eq!(count_params("([Ljava/lang/String;)V"), 1);
        assert_eq!(count_params("([[I)V"), 1);
    }

    // ── ClassOrigin ───────────────────────────────────────────────────────────

    #[test]
    fn test_origin_jar_vs_source() {
        let jar_origin = ClassOrigin::Jar(Arc::from("/path/to/lib.jar"));
        let src_origin = ClassOrigin::SourceFile(Arc::from("file:///Foo.java"));

        let mut idx = GlobalIndex::new();
        idx.add_classes(vec![
            make_class("com/lib", "Helper", jar_origin.clone()),
            make_class("com/app", "MyClass", src_origin.clone()),
        ]);

        // 删除 jar 来源的类，源文件类保留
        idx.remove_by_origin(&jar_origin);
        assert!(idx.get_class("com/lib/Helper").is_none());
        assert!(idx.get_class("com/app/MyClass").is_some());
    }

    #[test]
    fn test_multiple_jars_independent_origins() {
        let jar_a = ClassOrigin::Jar(Arc::from("a.jar"));
        let jar_b = ClassOrigin::Jar(Arc::from("b.jar"));

        let mut idx = GlobalIndex::new();
        idx.add_classes(vec![
            make_class("com/a", "A", jar_a.clone()),
            make_class("com/b", "B", jar_b.clone()),
        ]);

        idx.remove_by_origin(&jar_a);
        assert!(idx.get_class("com/a/A").is_none());
        assert!(idx.get_class("com/b/B").is_some());
    }

    // ── 与 source 解析集成 ────────────────────────────────────────────────────

    #[test]
    fn test_index_and_query_java_source() {
        use crate::index::codebase::index_source_text;

        let src = r#"
package com.example;
public class Calculator {
    private int result;
    public int add(int a, int b) { return a + b; }
    public int getResult() { return result; }
}
"#;
        let classes = index_source_text("file:///Calculator.java", src, "java", None);
        assert_eq!(classes.len(), 1);

        let mut idx = GlobalIndex::new();
        idx.add_classes(classes);

        let cls = idx.get_class("com/example/Calculator").unwrap();
        assert_eq!(cls.name.as_ref(), "Calculator");
        assert!(cls.methods.iter().any(|m| m.name.as_ref() == "add"));
        assert!(cls.methods.iter().any(|m| m.name.as_ref() == "getResult"));
        assert!(cls.fields.iter().any(|f| f.name.as_ref() == "result"));

        // 通过包查询
        let pkg_classes = idx.classes_in_package("com/example");
        assert_eq!(pkg_classes.len(), 1);
    }

    #[test]
    fn test_index_and_query_kotlin_source() {
        use crate::index::codebase::index_source_text;

        let src = r#"
package com.example
class UserService(val repo: String) {
    fun findUser(id: Int): String = ""
    fun deleteUser(id: Int) {}
}
object UserFactory {
    fun create(): UserService = UserService("")
}
"#;
        let classes = index_source_text("file:///UserService.kt", src, "kotlin", None);
        // UserService + UserFactory
        assert!(
            classes.iter().any(|c| c.name.as_ref() == "UserService"),
            "classes: {:?}",
            classes.iter().map(|c| c.name.as_ref()).collect::<Vec<_>>()
        );
        assert!(classes.iter().any(|c| c.name.as_ref() == "UserFactory"));

        let mut idx = GlobalIndex::new();
        idx.add_classes(classes);

        // 查 UserService
        let svc = idx.get_class("com/example/UserService").unwrap();
        assert!(svc.methods.iter().any(|m| m.name.as_ref() == "findUser"));
        assert!(svc.methods.iter().any(|m| m.name.as_ref() == "deleteUser"));

        // 通配符
        let pkg = idx.classes_in_package("com/example");
        assert_eq!(pkg.len(), 2);
    }

    #[test]
    fn test_source_overrides_bytecode_same_class() {
        // 模拟：jar 里有字节码，同时有源文件，源文件优先
        let jar_origin = ClassOrigin::Jar(Arc::from("mylib.jar"));
        let src_origin = ClassOrigin::SourceFile(Arc::from("file:///Foo.java"));

        let mut bytecode_class = make_class("com/example", "Foo", jar_origin);
        bytecode_class
            .methods
            .push(make_method("fromBytecode", "()V"));

        let mut source_class = make_class("com/example", "Foo", src_origin);
        source_class.methods.push(make_method("fromSource", "()V"));

        // 模拟 index_jar 的行为：source 先加入，bytecode 跳过同名
        let mut idx = GlobalIndex::new();
        idx.add_classes(vec![source_class]); // 源文件先

        // 字节码版本：如果 internal_name 已存在则不覆盖
        // （index_jar 里用 source_internal_names 集合过滤）
        if idx.get_class("com/example/Foo").is_none() {
            idx.add_classes(vec![bytecode_class]);
        }

        let cls = idx.get_class("com/example/Foo").unwrap();
        assert!(
            cls.methods.iter().any(|m| m.name.as_ref() == "fromSource"),
            "source version should win"
        );
        assert!(
            !cls.methods
                .iter()
                .any(|m| m.name.as_ref() == "fromBytecode")
        );
    }

    #[test]
    fn test_member_completion_across_files() {
        // 模拟两个文件的场景：Main.java 和 RandomClass.java
        use crate::index::codebase::index_source_text;

        let random_class_src = r#"
package org.cubewhy;
public class RandomClass {
    public void f() {}
}
"#;
        let main_src = r#"
package org.cubewhy.a;
import org.cubewhy.*;
class Main {
    public static void main() {
        RandomClass cl = new RandomClass();
        cl.f();
    }
}
"#;

        let mut idx = GlobalIndex::new();
        // 先 index RandomClass
        idx.add_classes(index_source_text(
            "file:///RandomClass.java",
            random_class_src,
            "java",
            None,
        ));
        // 再 index Main
        idx.add_classes(index_source_text(
            "file:///Main.java",
            main_src,
            "java",
            None,
        ));

        // 验证 RandomClass 在索引里
        let cls = idx.get_class("org/cubewhy/RandomClass");
        assert!(cls.is_some(), "RandomClass should be indexed");
        let cls = cls.unwrap();
        assert!(
            cls.methods.iter().any(|m| m.name.as_ref() == "f"),
            "f() should be in RandomClass"
        );

        // 验证通配符 import 展开能找到 RandomClass
        let resolved = idx.resolve_imports(&["org.cubewhy.*".into()]);
        assert!(
            resolved.iter().any(|c| c.name.as_ref() == "RandomClass"),
            "wildcard import should resolve RandomClass: {:?}",
            resolved.iter().map(|c| c.name.as_ref()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_mro_walks_superclass() {
        let mut idx = GlobalIndex::new();
        let mut parent = make_class("com/example", "Parent", ClassOrigin::Unknown);
        parent.methods.push(make_method("parentMethod", "()V"));
        let mut child = make_class("com/example", "Child", ClassOrigin::Unknown);
        child.super_name = Some("com/example/Parent".into());
        child.methods.push(make_method("childMethod", "()V"));
        idx.add_classes(vec![parent, child]);

        let (methods, _) = idx.collect_inherited_members("com/example/Child");
        assert!(methods.iter().any(|m| m.name.as_ref() == "childMethod"));
        assert!(
            methods.iter().any(|m| m.name.as_ref() == "parentMethod"),
            "parentMethod should be inherited"
        );
    }

    #[test]
    fn test_has_classes_in_package() {
        let mut idx = GlobalIndex::new();
        idx.add_classes(vec![make_class(
            "java/lang",
            "String",
            ClassOrigin::Unknown,
        )]);
        assert!(!idx.has_classes_in_package("java"));
        assert!(idx.has_classes_in_package("java/lang"));
    }

    #[test]
    fn test_param_names_from_java_source() {
        use crate::index::codebase::index_source_text;
        let src = r#"
package com.example;
public class Calc {
    public int add(int a, int b) { return a + b; }
}
"#;
        let classes = index_source_text("file:///Calc.java", src, "java", None);
        let method = classes[0]
            .methods
            .iter()
            .find(|m| m.name.as_ref() == "add")
            .unwrap();
        assert_eq!(method.params.items[0].name.as_ref(), "a");
        assert_eq!(method.params.items[1].name.as_ref(), "b");
    }

    #[test]
    fn test_merge_source_param_names_for_erased_generic_params() {
        let mut bytecode_class =
            make_class("com/example", "Box", ClassOrigin::Jar(Arc::from("x.jar")));
        bytecode_class.methods.push(MethodSummary {
            name: Arc::from("add"),
            access_flags: ACC_PUBLIC,
            is_synthetic: false,
            params: MethodParams::from_method_descriptor("(Ljava/lang/Object;)Z"),
            annotations: vec![],
            generic_signature: Some(Arc::from("(TE;)Z")),
            return_type: parse_return_type_from_descriptor("(Ljava/lang/Object;)Z"),
        });
        bytecode_class.methods.push(MethodSummary {
            name: Arc::from("add"),
            access_flags: ACC_PUBLIC,
            is_synthetic: false,
            params: MethodParams::from_method_descriptor("(ILjava/lang/Object;)V"),
            annotations: vec![],
            generic_signature: Some(Arc::from("(ITE;)V")),
            return_type: parse_return_type_from_descriptor("(ILjava/lang/Object;)V"),
        });

        let mut source_class = make_class(
            "com/example",
            "Box",
            ClassOrigin::SourceFile(Arc::from("file:///Box.java")),
        );
        source_class.methods.push(MethodSummary {
            name: Arc::from("add"),
            access_flags: ACC_PUBLIC,
            is_synthetic: false,
            params: MethodParams::from([("LE;", "e")]),
            annotations: vec![],
            generic_signature: None,
            return_type: parse_return_type_from_descriptor("(LE;)Z"),
        });
        source_class.methods.push(MethodSummary {
            name: Arc::from("add"),
            access_flags: ACC_PUBLIC,
            is_synthetic: false,
            params: MethodParams::from([("I", "index"), ("LE;", "element")]),
            annotations: vec![],
            generic_signature: None,
            return_type: parse_return_type_from_descriptor("(ILE;)V"),
        });

        let mut bytecode = vec![bytecode_class];
        merge_source_into_bytecode(&mut bytecode, vec![source_class]);

        let merged = &bytecode[0];
        let add_one = merged
            .methods
            .iter()
            .find(|m| m.name.as_ref() == "add" && m.desc().as_ref() == "(Ljava/lang/Object;)Z")
            .unwrap();
        assert_eq!(add_one.params.items[0].name.as_ref(), "e");

        let add_two = merged
            .methods
            .iter()
            .find(|m| m.name.as_ref() == "add" && m.desc().as_ref() == "(ILjava/lang/Object;)V")
            .unwrap();
        assert_eq!(add_two.params.items[0].name.as_ref(), "index");
        assert_eq!(add_two.params.items[1].name.as_ref(), "element");
    }

    #[test]
    fn test_strict_source_name_generation() {
        // 场景 1：合法的嵌套类 (内部有明确的 inner_class_of)
        let mut nested = make_class("com/example", "Outer$Inner", ClassOrigin::Unknown);
        nested.inner_class_of = Some(Arc::from("Outer"));
        assert_eq!(nested.source_name(), "com.example.Outer.Inner");

        // 场景 2：带 $ 的普通类 (比如混淆后的类 a$b)
        let obfuscated = make_class("com/example", "a$b", ClassOrigin::Unknown);
        // 注意：没有设置 inner_class_of，所以不认为是嵌套类
        assert_eq!(obfuscated.source_name(), "com.example.a$b");

        // 场景 3：普通顶层类
        let normal = make_class("java/lang", "String", ClassOrigin::Unknown);
        assert_eq!(normal.source_name(), "java.lang.String");
    }
}
