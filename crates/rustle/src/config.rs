pub const APP_ID: &str = "io.github.turbinebmw.Rustle";
pub const RESOURCE_PATH: &str = "/io/github/turbinebmw/Rustle";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// The schemas compiled by build.rs, used when the app isn't installed.
pub const BUILT_SCHEMA_DIR: &str = concat!(env!("OUT_DIR"), "/schemas");
pub const GETTEXT_DOMAIN: &str = "rustle";
