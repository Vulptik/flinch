use super::*;

#[test]
fn configuration_dialog_is_configured_for_json_and_cancellation_retains_native_failure_codes() {
  for export in [false, true] {
    let result = pick_configuration_with(export, "Isolated configuration fixture", |dialog| {
      // SAFETY: inspect the worker-owned COM dialog without invoking its Show method.
      let flags = unsafe { dialog.GetOptions() }.unwrap();
      assert!(flags.contains(FOS_FORCEFILESYSTEM) && flags.contains(FOS_PATHMUSTEXIST));
      assert_eq!(flags.contains(FOS_OVERWRITEPROMPT), export);
      assert_eq!(flags.contains(FOS_FILEMUSTEXIST), !export);
      assert_eq!(dialog.cast::<IFileSaveDialog>().is_ok(), export);
      if export {
        // SAFETY: copy and release the COM-owned filename before this dialog is dropped.
        unsafe {
          let name = dialog.GetFileName().unwrap();
          let text = name.to_string().unwrap();
          CoTaskMemFree(Some(name.0.cast()));
          assert_eq!(text, "config.json");
        }
      }
      select_configuration_with(dialog, |_| {
        Err(windows::core::Error::from_hresult(windows::core::HRESULT(
          0x800704c7u32 as i32,
        )))
      })
    })
    .unwrap();
    assert!(result.is_none());
    let error = pick_configuration_with(export, "Failure fixture", |dialog| {
      select_configuration_with(dialog, |_| {
        Err(windows::core::Error::from_hresult(windows::core::HRESULT(
          0x80070005u32 as i32,
        )))
      })
    })
    .unwrap_err();
    assert_eq!(
      error
        .downcast_ref::<windows::core::Error>()
        .unwrap()
        .code()
        .0 as u32,
      0x80070005
    );
  }
}

#[test]
fn toast_xml_preserves_untrusted_text_without_creating_markup_or_showing_a_notification() {
  let _apartment = Apartment::new().unwrap();
  let title = "<script>& \"capture\"";
  let body = "C:\\captures\\<text>猫 & é.png\nOriginal error";
  let document = toast_document(title, body).unwrap();
  let fields = document
    .GetElementsByTagName(&HSTRING::from("text"))
    .unwrap();
  assert_eq!(fields.Length().unwrap(), 2);
  assert_eq!(fields.Item(0).unwrap().InnerText().unwrap(), title);
  assert_eq!(fields.Item(1).unwrap().InnerText().unwrap(), body);
  assert_eq!(
    document
      .GetElementsByTagName(&HSTRING::from("script"))
      .unwrap()
      .Length()
      .unwrap(),
    0
  );
  let xml = document.GetXml().unwrap().to_string();
  assert!(xml.contains("&lt;script&gt;&amp;"));
  assert!(xml.contains("Original error"));
}
