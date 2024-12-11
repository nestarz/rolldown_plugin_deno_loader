use rolldown_fs::{FileSystem, OsFileSystem};
use serde::Deserialize;
use std::borrow::Cow;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use rolldown_common::ModuleType;
use rolldown_plugin::{
  HookLoadArgs, HookLoadOutput, HookLoadReturn, HookResolveIdArgs, HookResolveIdOutput,
  HookResolveIdReturn, Plugin, PluginContext, PluginContextResolveOptions,
};

use import_map::parse_from_json;

#[derive(Debug, Default)]
pub struct DenoLoaderPlugin;

#[derive(Deserialize, Debug)]
#[serde(tag = "kind")]
enum ModuleInfo {
  #[serde(rename = "esm")]
  Esm {
    local: String,
    specifier: String,
    #[serde(rename = "mediaType")]
    media_type: DenoMediaType,
  },
  #[serde(rename = "npm")]
  Npm {
    specifier: String,
    #[serde(rename = "npmPackage")]
    npm_package: String,
  },
}

#[derive(Deserialize, Debug)]
enum DenoMediaType {
  TypeScript,
  Tsx,
  JavaScript,
  Jsx,
  Json,
  Dmts,
  Mjs,
}

#[derive(Deserialize, Debug)]
struct DenoInfoJsonV1 {
  redirects: HashMap<String, String>,
  modules: Vec<ModuleInfo>,
}

fn follow_redirects(
  initial: &str,
  redirects: &HashMap<String, String>,
) -> Result<String, &'static str> {
  let mut current = initial.to_string();
  let mut seen = std::collections::HashSet::new();

  while let Some(next) = redirects.get(&current) {
    if !seen.insert(current.clone()) {
      return Err("Circular redirect detected");
    }
    current = next.clone();
  }

  Ok(current)
}

fn get_deno_info(specifier: &str) -> Result<DenoInfoJsonV1, &'static str> {
  let output = std::process::Command::new("deno")
    .args(["info", "--json", specifier])
    .output()
    .expect("Failed to execute deno info command");

  if !output.status.success() {
    return Err("deno info command failed");
  }

  Ok(serde_json::from_slice(&output.stdout).expect("Failed to parse JSON output"))
}

pub fn get_local_path(specifier: &str) -> Result<String, &'static str> {
  let info: DenoInfoJsonV1 = get_deno_info(specifier)?;

  // Follow redirects to get the final specifier
  let final_specifier = follow_redirects(specifier, &info.redirects)?;
  println!("specifier: {}, final_specifier: {}", specifier, final_specifier);

  // Find module with the final specifier
  info
    .modules
    .into_iter()
    .find_map(|m| match m {
      ModuleInfo::Esm { specifier, local, .. } if specifier == final_specifier => Some(local),
      _ => None,
    })
    .ok_or_else(|| "Module not found or has no local path")
}

impl Plugin for DenoLoaderPlugin {
  fn name(&self) -> Cow<'static, str> {
    "rolldown:data-url".into()
  }

  fn resolve_id(
    &self,
    ctx: &PluginContext,
    args: &HookResolveIdArgs<'_>,
  ) -> impl std::future::Future<Output = HookResolveIdReturn> {
    async {
      let id = if args.specifier.starts_with('.') {
        args
          .importer
          .and_then(|importer| url::Url::parse(importer).ok())
          .and_then(|base_url| base_url.join(&args.specifier).ok())
          .map(|joined_url| {
            if joined_url.scheme() == "file" {
              joined_url.path().to_string()
            } else {
              joined_url.to_string()
            }
          })
          .unwrap_or_else(|| args.specifier.to_string())
      } else {
        args.specifier.to_string()
      };

      let base_url = ctx
        .cwd()
        .to_str()
        .and_then(|s| url::Url::from_file_path(s).ok())
        .unwrap_or_else(|| url::Url::parse("file:///").unwrap());

      let import_map =
        parse_from_json(base_url.clone(), r#"{"imports": { "@std/assert": "jsr:@std/assert" }}"#)
          .unwrap()
          .import_map;

      let maybe_resolved = import_map
        .resolve(&id, &base_url)
        .ok()
        .map(|url| url.to_string())
        .unwrap_or_else(|| id.to_string());

      println!("specifier: {}, id: {}, maybe_resolved: {}", args.specifier, id, maybe_resolved);

      if maybe_resolved.starts_with("jsr:") {
        let info: DenoInfoJsonV1 = get_deno_info(&maybe_resolved).expect("get info failed");
        let final_specifier =
          follow_redirects(&maybe_resolved, &info.redirects).expect("follow_redirects failed");

        return Ok(Some(HookResolveIdOutput {
          id: final_specifier,
          external: Some(false),
          ..Default::default()
        }));
      } else if maybe_resolved.starts_with("http:") || maybe_resolved.starts_with("https:") {
        return Ok(Some(HookResolveIdOutput {
          id: maybe_resolved.to_string(),
          external: Some(false),
          ..Default::default()
        }));
      } else if maybe_resolved.starts_with("npm:") {
        let info: DenoInfoJsonV1 = get_deno_info(&maybe_resolved).expect("get info failed");
        let redirected =
          follow_redirects(&maybe_resolved, &info.redirects).expect("follow_redirects failed");

        if let Some(ModuleInfo::Npm { npm_package, .. }) = info
          .modules
          .into_iter()
          .find(|m| matches!(m, ModuleInfo::Npm { specifier, .. } if specifier == &redirected))
        {
          let package_name = npm_package.split('@').next().unwrap_or(&npm_package).to_string();
          return Ok(
            ctx
              .resolve(
                &package_name,
                args.importer,
                Some(PluginContextResolveOptions {
                  import_kind: args.kind,
                  skip_self: true,
                  custom: Arc::clone(&args.custom),
                }),
              )
              .await?
              .map(|resolved_id| {
                Some(HookResolveIdOutput { id: resolved_id.id.to_string(), ..Default::default() })
              })?,
          );
        }
      }
      Ok(None)
    }
  }

  fn load(
    &self,
    _ctx: &PluginContext,
    args: &HookLoadArgs<'_>,
  ) -> impl std::future::Future<Output = HookLoadReturn> + Send {
    async {
      println!("test {}", args.id);
      if args.id.starts_with("jsr:")
        || args.id.starts_with("http:")
        || args.id.starts_with("https:")
      {
        let local_path: String = get_local_path(args.id).expect("local path not found");
        println!("local {}", local_path);
        // Return the specifier as the id to tell rolldown that this data url is handled by the plugin. Don't fallback to
        // the default resolve behavior and mark it as external.
        Ok(Some(HookLoadOutput {
          code: String::from_utf8_lossy(
            &OsFileSystem::read(&OsFileSystem, Path::new(&local_path))
              .expect("cant read local path"),
          )
          .into_owned(),
          module_type: Some(ModuleType::Tsx),
          ..Default::default()
        }))
      } else {
        Ok(None)
      }
    }
  }
}