//! Omp Coding Agent hook handlers — argv-based lifecycle plus TypeScript plugin.

mod handlers;
mod plugin;

#[cfg(test)]
mod tests;

pub use handlers::dispatch_omp_hook;
pub use plugin::{
    PLUGIN_SOURCE, ensure_omp_plugin_installed, extension_inject_args, get_omp_plugin_path,
    install_omp_plugin, remove_omp_plugin, strip_managed_extension_args,
    verify_omp_plugin_installed,
};

#[cfg(test)]
pub(crate) use handlers::{
    handle_start, handle_status, handle_stop, upsert_plugin_notify_endpoint,
};
