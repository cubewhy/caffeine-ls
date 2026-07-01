use serde::Deserialize;

/// Represents the global project modules index (.idea/modules.xml)
#[derive(Debug, Deserialize)]
#[serde(rename = "project")]
pub struct IdeaModulesXml {
    #[serde(rename = "component", default)]
    pub components: Vec<IdeaModulesComponent>,
}

#[derive(Debug, Deserialize)]
pub struct IdeaModulesComponent {
    #[serde(rename = "@name")]
    pub name: String,
    #[serde(rename = "modules")]
    pub modules: Option<IdeaModulesList>,
}

#[derive(Debug, Deserialize)]
pub struct IdeaModulesList {
    #[serde(rename = "module", default)]
    pub items: Vec<IdeaModuleRef>,
}

#[derive(Debug, Deserialize)]
pub struct IdeaModuleRef {
    #[serde(rename = "@filepath")]
    pub file_path: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename = "project")]
pub struct IdeaMiscXml {
    #[serde(rename = "component", default)]
    pub components: Vec<IdeaMiscComponent>,
}

#[derive(Debug, Deserialize)]
pub struct IdeaMiscComponent {
    #[serde(rename = "@name")]
    pub name: String,
    #[serde(rename = "@languageLevel")]
    pub language_level: Option<String>, // Captures "JDK_25", "JDK_1_8", etc.
}

/// Represents individual shared library XML configuration files inside .idea/libraries/*.xml
#[derive(Debug, Deserialize)]
#[serde(rename = "component")]
pub struct IdeaLibraryTableXml {
    #[serde(rename = "library", default)]
    pub libraries: Vec<IdeaProjectLibrary>,
}

#[derive(Debug, Deserialize)]
pub struct IdeaProjectLibrary {
    #[serde(rename = "@name")]
    pub name: String,
    #[serde(rename = "CLASSES")]
    pub classes: Option<IdeaLibraryClasses>,
    #[serde(rename = "jarDirectory", default)]
    pub jar_directories: Vec<IdeaJarDirectory>,
}

/// Represents an individual module configuration (.iml file)
#[derive(Debug, Deserialize)]
#[serde(rename = "module")]
pub struct IdeaModuleDoc {
    #[serde(rename = "component", default)]
    pub components: Vec<IdeaComponent>,
}

#[derive(Debug, Deserialize)]
pub struct IdeaComponent {
    #[serde(rename = "@name")]
    pub name: String,

    #[serde(rename = "content", default)]
    pub contents: Vec<IdeaContentRoot>,

    #[serde(rename = "orderEntry", default)]
    pub order_entries: Vec<IdeaOrderEntry>,
}

#[derive(Debug, Deserialize)]
pub struct IdeaContentRoot {
    #[serde(rename = "@url")]
    pub url: String,

    #[serde(rename = "sourceFolder", default)]
    pub source_folders: Vec<IdeaSourceFolder>,
}

#[derive(Debug, Deserialize)]
pub struct IdeaSourceFolder {
    #[serde(rename = "@url")]
    pub url: String,

    #[serde(rename = "@isTestSource")]
    pub is_test_source: Option<String>,

    #[serde(rename = "@generated")]
    pub generated: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct IdeaOrderEntry {
    #[serde(rename = "@type")]
    pub entry_type: String,

    #[serde(rename = "@name")]
    pub name: Option<String>,

    #[serde(rename = "@module-name")]
    pub module_name: Option<String>,

    #[serde(rename = "library")]
    pub inline_library: Option<IdeaInlineLibrary>,
}

#[derive(Debug, Deserialize)]
pub struct IdeaInlineLibrary {
    #[serde(rename = "CLASSES")]
    pub classes: Option<IdeaLibraryClasses>,
    #[serde(rename = "jarDirectory", default)]
    pub jar_directories: Vec<IdeaJarDirectory>,
}

#[derive(Debug, Deserialize)]
pub struct IdeaLibraryClasses {
    #[serde(rename = "root", default)]
    pub roots: Vec<IdeaLibraryRoot>,
}

#[derive(Debug, Deserialize)]
pub struct IdeaLibraryRoot {
    #[serde(rename = "@url")]
    pub url: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct IdeaJarDirectory {
    #[serde(rename = "@url")]
    pub url: String,
    #[serde(rename = "@recursive")]
    pub recursive: Option<String>,
}
