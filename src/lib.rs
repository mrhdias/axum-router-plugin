//! # Plugin-Based Axum Server
//!
//! This module provides functionality for dynamically loading and handling plugins in an Axum-based server.
//! It allows plugins to define routes and functionality without the need to recompile the application
//! whenever a plugin is activated or deactivated. Plugin management is handled through a configuration file,
//! providing flexibility for dynamic behavior.
//!
//! The system is designed to facilitate extensibility by loading plugins that implement specific
//! HTTP routes and handling request and response transformations, such as header manipulation and
//! JSON response parsing.
//!
//! ## Key Features:
//! - Load shared libraries as plugins.
//! - Routes and functions from plugins are integrated into the Axum router.
//! - Plugins can be enabled or disabled via a configuration file (`plugin.json`).
//! - No need to recompile the main application to activate or deactivate a plugin.
//! - **Note:** After adding, enabling, or disabling one or more plugins, it is necessary to restart the server
//!   for the changes to take effect.
//!
//! ## Plugin Configuration:
//! Each plugin inside the `plugins` directory must include a `plugins.json` file. This file specifies the library path, version, and whether the plugin is enabled.
//! 
//! Example `plugin.json` entry:
//! ```json
//! {
//!   "name": "plugin_name",
//!   "description": "Axum Router Plugin Example",
//!   "lib_path": "./path/to/plugin.so",
//!   "version": "0.1.0",
//!   "license": "MIT",
//!   "enabled": true
//! }
//! ```
//!
//! ## Example Usage
//! ```rust,no_run
//! use axum_router_plugin::Plugins;
//! use axum::{
//!     routing::get,
//!     Router,
//! };
//!
//! #[tokio::main]
//! async fn main() {
//!     // Load plugins from the plugins directory.
//!     // Each plugin must have its own directory containing a plugin.json file
//!     // that provides information about the plugin, such as the library path,
//!     // version, and whether it's enabled.
//!     //
//!     // You can change the location of the plugins directory by setting
//!     // the environment variable PLUGINS_DIR, for example:
//!     // export PLUGINS_DIR=path/to/plugins
//!     //
//!     // Set the argument to true if you want to add the plugin name to the routes.
//!     let axum_plugins = Plugins::new(Some(true));
//!
//!     // Load the plugins and create a router with the loaded plugins.
//!     // If loading fails, the program will panic with an error message.
//!     let plugins_router = match axum_plugins.load() {
//!         Ok(router) => router,
//!         Err(err) => panic!("Error loading plugins: {}", err),
//!     };
//!
//!     // Build our application with a route.
//!     // The plugins are nested under the "/plugin" path.
//!     let _app = Router::new()
//!         .route("/", get(|| async {
//!             "Hello world!"
//!         }))
//!         .nest("/plugin", plugins_router);
//! }
//! ```
//!
//! This example demonstrates how to load plugins dynamically at runtime, configure routes, and nest plugin routes under a specified path.
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use axum::{
    extract::RawQuery,
    response::{Html, Json, IntoResponse},
    routing::{get, post},
    Router,
};
use hyper::{HeaderMap, header::HeaderValue};
use libloading::{Library, Symbol};
use std::ffi::{c_char, CStr, CString};
use once_cell::sync::Lazy;
use std::sync::Mutex;

/// Describes a plugin route configuration, which includes:
/// - `path`: The URL path to handle.
/// - `function`: The name of the function in the plugin.
/// - `method_router`: The HTTP method (GET, POST) for this route.
/// - `response_type`: Specifies the response format (e.g., `text`, `html`, `json`).
#[derive(Debug, Deserialize)]
struct PluginRoute {
    path: String,
    function: String,
    method_router: String,
    response_type: String,
}

/// Defines a plugin, with metadata such as:
/// - `name`: The plugin name.
/// - `version`: The plugin version.
/// - `path`: The file system path to the shared library.
/// - `enabled`: Indicates whether the plugin is enabled.
#[derive(Debug, Clone, Deserialize)]
struct Plugin {
    name: String,
    // description: Option<String>,
    version: String,
    // license: Option<String>,
    lib_path: String,
    enabled: bool,
}

/// Struct for managing plugin loading, routing, and naming behavior.
#[derive(Deserialize, Debug)]
pub struct Plugins {
    name_to_route: bool,
}

/// A global flag to enable or disable debug output, based on the `DEBUG` environment variable.
static DEBUG: Lazy<bool> = Lazy::new(|| {
    std::env::var("DEBUG")
        .map(|val| val == "true")
        .unwrap_or(false)
});

/// A global map that stores loaded plugin libraries, with the library protected by a `Mutex` to
/// allow safe concurrent access.
static LIBRARIES: Lazy<HashMap<String, Mutex<Library>>> = Lazy::new(|| {

    let plugins_dir = std::env::var("PLUGINS_DIR")
        .map(|val| val.is_empty()
            .then_some("plugins".to_string()
        )
        .or(Some(val)).unwrap())
        .unwrap_or("plugins".to_string());

    let plugins_path = std::path::Path::new(&plugins_dir);
    if !plugins_path.is_dir() {
        eprintln!("Error: PLUGINS_DIR does not exist: {}", plugins_dir);
        std::process::exit(1);
    }

    println!("Load plugins from: {}", plugins_dir);

    let mut libraries = HashMap::new();

    for entry in std::fs::read_dir(plugins_path).unwrap() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                eprintln!("Error reading plugin directory entry: {}", e);
                continue;
            }
        };

        let path_dir = entry.path();
        if path_dir.is_dir() {
            println!("DIR: {}", path_dir.display());
            let plugin_conf_path = path_dir.join("plugin.json");
            if!plugin_conf_path.is_file() {
                eprintln!("Error: Missing plugin.json in: {}", path_dir.display());
                continue;
            }

            let file = std::fs::File::open(plugin_conf_path).unwrap();
            let reader = std::io::BufReader::new(file);
        
            // Deserialize the JSON data into the struct
            let plugin_conf: Plugin = match serde_json::from_reader(reader) {
                Ok(config) => config,
                Err(e) => {
                    eprintln!("Error parsing plugin.json: {}", e);
                    continue;
                }
            };

            // Skip disabled plugins
            if !plugin_conf.enabled {
                eprintln!(
                    "Skipping plugin: {}: {} - disabled", 
                    plugin_conf.name, path_dir.display()
                );
                continue;
            }

            if plugin_conf.lib_path.is_empty() {
                eprintln!(
                    "Skipping plugin: {}: {} - no shared library path specified", 
                    plugin_conf.name, path_dir.display()
                );
                continue;
            }

            let lib_path = plugin_conf.lib_path.starts_with('/')
                .then_some(std::path::PathBuf::new().join(&plugin_conf.lib_path))
                .or(Some(path_dir.join(&plugin_conf.lib_path))).unwrap();

            if !lib_path.is_file() {
                eprintln!(
                    "Skipping plugin: {}: {} - shared library not found", 
                    plugin_conf.name, path_dir.display()
                );
                continue;
            }

            let lib = unsafe {
                match Library::new(&lib_path) {
                    Ok(lib) => lib,
                    Err(e) => panic!("Error loading library {}: {}", lib_path.display(), e),
                }
            };
    
            println!("Plugin loaded: {} Version: {}", plugin_conf.name, plugin_conf.version);
    
            libraries.insert(plugin_conf.name, Mutex::new(lib));
        }
    }

    libraries
});

impl Plugins {

    /// Creates a new instance of the `Plugins` struct.
    ///
    /// # Arguments
    /// * `name_to_route` - An optional boolean indicating whether to prepend the plugin name to each route.
    ///
    /// # Returns
    /// A new `Plugins` instance.
    pub fn new(
        name_to_route: Option<bool>,
    ) -> Self {

        Plugins {
            name_to_route: match name_to_route {
                Some(true) => true,
                Some(false) => false,
                None => false,
            },
        }
    }

    /// Handles the execution of a plugin's function, passing headers and body as arguments.
    /// The function is executed in a blocking task, and memory is managed for the returned C string.
    ///
    /// # Arguments
    /// * `headers` - The request headers.
    /// * `body` - The request body as a string.
    /// * `function` - A pointer to the plugin's function to execute.
    /// * `free` - A pointer to the plugin's memory-freeing function.
    ///
    /// # Returns
    /// The response as a string.
    async fn handle_route(
        headers: HeaderMap,
        body: String,
        function: extern "C" fn(*mut HeaderMap, *const c_char) -> *const c_char,
        free: extern "C" fn(*mut c_char),
    ) -> String {

        if *DEBUG { println!("Handle Route Header Map: {:?}", headers); }

        tokio::task::spawn_blocking(move || -> String {
            // Box the headers and convert the body to a CString
            let box_headers = Box::new(headers);
            let c_body = CString::new(body).unwrap();
    
            // Call the external C function with the appropriate pointers
            let ptr = function(Box::into_raw(box_headers), c_body.as_ptr());
            if ptr.is_null() {
                panic!("Received null pointer from function");
            }

            // clean this from memory
            unsafe {
                let data = CStr::from_ptr(ptr).to_string_lossy().into_owned();
                free(ptr as *mut c_char);
                data
            }
        }).await.unwrap()
    }

    /// Sets the appropriate response type (text, HTML, JSON) based on the `response_type` argument.
    ///
    /// # Arguments
    /// * `response` - The raw response string.
    /// * `response_type` - The expected format of the response.
    ///
    /// # Returns
    /// An Axum response.
    fn set_response(
        response: &str,
        response_type: &str,
    ) -> axum::response::Response {

        match response_type.to_lowercase().as_str() {
            "text" => response.to_string()
                .into_response(),
            "html" => Html(response.to_string())
                .into_response(),
            "json" => {
                // println!("Json String Response : {}", response.to_string());
                let v: Value = match serde_json::from_str(response) {
                    Ok(json_value) => json_value,
                    Err(e) => {
                        eprintln!("Error parsing JSON: {}", e);
                        serde_json::Value::String(format!("Error parsing JSON: {}", e))
                    },
                };
                Json(v).into_response()
            },
            _ => panic!("Unsupported response format"),
        }
    }

    /// Loads and merges routes from all enabled plugins into an Axum `Router`.
    ///
    /// # Returns
    /// A result containing the constructed router or an error if a plugin fails to load.
    pub fn load(&self) -> Result<Router, libloading::Error> {

        let message = || -> String {
            let count = LIBRARIES.len();
            format!("Loaded plugins: {}", count)
        }();

        let mut router: Router = Router::new()
            .route("/", get(|| async {
                message
            })
        );
        
        if LIBRARIES.is_empty() {
            return Ok(router);
        }

        for (name, lib) in LIBRARIES.iter() {

            let lib = match lib.lock() {
                Ok(lib) => lib,
                Err(e) => panic!("Error locking library: {}", e),
            };

            let routes_fn: Symbol<extern "C" fn() -> *const c_char> = unsafe {
                match lib.get(b"routes\0") {
                    Ok(symbol) => symbol,
                    Err(e) =>  panic!("Error getting routes: {}", e),
                }
            };

            let route_list_ptr = routes_fn();

            if route_list_ptr.is_null() {
                panic!("Received null pointer from routes function");
            }

            // clean this from memory
            let json_data = unsafe {
                CStr::from_ptr(route_list_ptr).to_string_lossy().into_owned()
            };

            // Clean up memory allocated by plugin if necessary
            let free_fn: Symbol<extern "C" fn(*mut c_char)> = unsafe {
                match lib.get(b"free\0") {
                    Ok(symbol) => symbol,
                    Err(e) => panic!("Error getting free function: {}", e),
                }
            };
        
            // Free the memory
            free_fn(route_list_ptr as *mut c_char);

            if *DEBUG { println!("Routes Json: {}", json_data); }

            let route_list: Vec<PluginRoute> = serde_json::from_str(&json_data).unwrap();

            for route in route_list {
                // Load the plugin_route_function

                let function: Symbol<extern "C" fn(*mut HeaderMap, *const c_char) -> *const c_char> = unsafe {
                    match lib.get(route.function.as_bytes()) {
                        Ok(symbol) => symbol,
                        Err(e) => panic!("Error getting plugin_route_function: {}", e),
                    }
                };

                // Move the loaded function into the closure to avoid borrowing `lib`
                let cloned_fn = *function;
                let cloned_free_fn = *free_fn;

                // check if route.path start with "/"
                let route_path = if self.name_to_route {
                    format!("/{}/{}", &name, if route.path.starts_with("/") {
                        &route.path[1..]
                    } else {
                        &route.path
                    })
                } else {
                    route.path
                };

                // https://docs.rs/axum/latest/axum/extract/index.html
                let r = Router::new()
                    .route(&route_path, match route.method_router.to_lowercase().as_str() {
                        "get" => get(move |
                            RawQuery(query): RawQuery,
                            mut headers: HeaderMap,
                            body: String,
                        | async move {
                            if let Some(query) = query {
                                headers.insert("x-raw-query", HeaderValue::from_str(&query).unwrap());
                            }
                            let response = Self::handle_route(
                                headers,
                                body, 
                                cloned_fn, 
                                cloned_free_fn,
                            ).await;
                            Self::set_response(&response, &route.response_type)
                        }),
                        "post" => post(move |
                            RawQuery(query): RawQuery,
                            mut headers: HeaderMap,
                            body: String,
                        | async move {
                            if let Some(query) = query {
                                headers.insert("x-raw-query", HeaderValue::from_str(&query).unwrap());
                            }
                            let response = Self::handle_route(
                                headers,
                                body, 
                                cloned_fn, 
                                cloned_free_fn,
                            ).await;
                            Self::set_response(&response, &route.response_type)
                        }),
                        _ => panic!("Unsupported method: {:?}", route.method_router),
                    }
                );
                router = router.merge(r);
            }
        }

        Ok(router)
    }
}