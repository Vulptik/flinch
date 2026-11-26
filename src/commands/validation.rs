use std::path::Path;

pub(crate) const CANCELLED: &str = "CANCELLED";

pub(crate) fn cancelled() -> String {
  CANCELLED.to_string()
}

pub(crate) fn ensure_size_limit(
  actual_bytes: u64,
  max_bytes: u64,
  label: &str,
) -> Result<(), String> {
  if actual_bytes > max_bytes {
    return Err(format!(
      "{} is too large ({}MB). Maximum size is {}MB.",
      label,
      actual_bytes / (1024 * 1024),
      max_bytes / (1024 * 1024)
    ));
  }

  Ok(())
}

pub(crate) fn ensure_path_exists(path: &Path, label: &str) -> Result<(), String> {
  if !path.exists() {
    return Err(format!("{label} does not exist: {}", path.display()));
  }

  Ok(())
}

pub(crate) fn ensure_known_value(is_known: bool, kind: &str, value: &str) -> Result<(), String> {
  if !is_known {
    return Err(format!("Unknown {kind}: {value}"));
  }

  Ok(())
}

pub(crate) fn ensure_absolute_non_unc_path(path: &str, operation: &str) -> Result<(), String> {
  let candidate = Path::new(path);
  if !candidate.is_absolute() || path.starts_with("\\\\") || path.starts_with("//") {
    return Err(format!("{operation}: rejected path: {path}"));
  }

  Ok(())
}

pub(crate) fn ensure_safe_open_target(path: &str, operation: &str) -> Result<(), String> {
  if path.starts_with("ms-windows-store:") {
    let identity: serde_json::Value =
      serde_json::from_str(include_str!("../../resources/store-identity.json"))
        .map_err(|error| super::ipc_context("Read Store identity", error))?;
    let product = identity["storeId"].as_str().ok_or_else(|| {
      super::ipc_context(
        "Read Store identity",
        anyhow::anyhow!("Store product ID is missing"),
      )
    })?;
    return ensure_known_value(
      path == format!("ms-windows-store://review/?ProductId={product}"),
      "Store review target",
      path,
    );
  }
  let is_https = path.starts_with("https://");
  if is_https {
    return Ok(());
  }

  ensure_absolute_non_unc_path(path, operation)?;
  ensure_path_exists(Path::new(path), "Target path")
}
