//! Presentation of the platform-release reports
//! ([JEP 247](https://openjdk.org/jeps/247)): the report line javac prints for
//! an API the compile release does not provide.
//!
//! The release check in the type layer detects the API and records it as a
//! [`ReleaseApi`]; the sentence it renders to lives here.

use hir_ty::TyDatabase;
use hir_ty::java::release_api::ReleaseApi;

pub(crate) fn render(db: &dyn TyDatabase, api: &ReleaseApi, found: u8, added: u8) -> String {
    let api = match api {
        ReleaseApi::Class { name } => format!("class '{}'", name.as_str()),
        ReleaseApi::Method {
            owner,
            name,
            params,
        } => {
            let params = params
                .iter()
                .map(|ty| ty.display(db).to_string())
                .collect::<Vec<_>>()
                .join(", ");
            if name == "<init>" {
                format!(
                    "constructor '{}({params})' in '{owner}'",
                    simple_name(owner)
                )
            } else {
                format!("method '{name}({params})' in '{owner}'")
            }
        }
        ReleaseApi::Field { owner, name } => format!("field '{name}' in '{owner}'"),
        ReleaseApi::Member { owner, name } => format!("member '{name}' in '{owner}'"),
    };
    format!("{api} is not supported in release {found} (added in release {added})")
}

fn simple_name(owner: &str) -> &str {
    owner.rsplit('.').next().unwrap_or(owner)
}
