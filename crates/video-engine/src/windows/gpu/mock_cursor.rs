//! View and constant-buffer doubles for the real pointer compositor's D3D calls.
use super::*;
#[implement(ID3D11ComputeShader)]
pub(super) struct CursorShader {
  pub state: Arc<State>,
}
#[implement(ID3D11Buffer)]
pub(super) struct CursorBuffer {
  pub state: Arc<State>,
  pub desc: D3D11_BUFFER_DESC,
}
#[implement(ID3D11ShaderResourceView, ID3D11UnorderedAccessView)]
pub(super) struct CursorView {
  pub state: Arc<State>,
  pub resource: ID3D11Resource,
}
macro_rules! child {
  ($kind:ident) => {
    impl ID3D11DeviceChild_Impl for $kind {
      fn GetDevice(&self, _: windows_core::OutRef<ID3D11Device>) {
        self.state.unexpected("Cursor.GetDevice");
      }
      fn GetPrivateData(
        &self,
        _: *const windows_core::GUID,
        _: *mut u32,
        _: *mut core::ffi::c_void,
      ) -> windows_core::Result<()> {
        self.state.unexpected("Cursor.GetPrivateData");
        Err(E_NOTIMPL.into())
      }
      fn SetPrivateData(
        &self,
        _: *const windows_core::GUID,
        _: u32,
        _: *const core::ffi::c_void,
      ) -> windows_core::Result<()> {
        self.state.unexpected("Cursor.SetPrivateData");
        Err(E_NOTIMPL.into())
      }
      fn SetPrivateDataInterface(
        &self,
        _: *const windows_core::GUID,
        _: windows_core::Ref<windows_core::IUnknown>,
      ) -> windows_core::Result<()> {
        self.state.unexpected("Cursor.SetPrivateDataInterface");
        Err(E_NOTIMPL.into())
      }
    }
  };
}
child!(CursorShader_Impl);
child!(CursorBuffer_Impl);
child!(CursorView_Impl);
impl ID3D11ComputeShader_Impl for CursorShader_Impl {}
impl ID3D11Resource_Impl for CursorBuffer_Impl {
  fn GetType(&self, out: *mut D3D11_RESOURCE_DIMENSION) {
    unsafe {
      *out = D3D11_RESOURCE_DIMENSION_BUFFER;
    }
  }
  fn SetEvictionPriority(&self, _: u32) {
    self.state.unexpected("Cursor.SetEvictionPriority");
  }
  fn GetEvictionPriority(&self) -> u32 {
    self.state.unexpected("Cursor.GetEvictionPriority");
    0
  }
}
impl ID3D11Buffer_Impl for CursorBuffer_Impl {
  fn GetDesc(&self, out: *mut D3D11_BUFFER_DESC) {
    unsafe {
      *out = self.desc;
    }
  }
}
impl ID3D11View_Impl for CursorView_Impl {
  fn GetResource(&self, out: windows_core::OutRef<ID3D11Resource>) {
    out
      .write(Some(self.resource.clone()))
      .expect("view resource output");
  }
}
impl ID3D11ShaderResourceView_Impl for CursorView_Impl {
  fn GetDesc(&self, _: *mut D3D11_SHADER_RESOURCE_VIEW_DESC) {
    self.state.unexpected("Cursor.SRV.GetDesc");
  }
}
impl ID3D11UnorderedAccessView_Impl for CursorView_Impl {
  fn GetDesc(&self, _: *mut D3D11_UNORDERED_ACCESS_VIEW_DESC) {
    self.state.unexpected("Cursor.UAV.GetDesc");
  }
}

#[test]
fn pointer_compositor_caches_shapes_uploads_only_metadata_and_unbinds_after_dispatch() {
  let (gpu, state) = fixture();
  let mut compositor = super::super::super::cursor::CursorCompositor::new(&gpu)
    .expect("compiled HLSL and fake GPU views");
  let before = state.creates.load(Ordering::Acquire);
  let mut shape = super::super::super::capture::PointerShape {
    kind: 2,
    width: 17,
    height: 9,
    pitch: 68,
    bytes: vec![255; 68 * 9],
    visible: true,
    x: -3,
    y: 4,
    generation: 1,
  };
  assert_eq!(
    compositor
      .compose(&gpu, &shape)
      .expect("color cursor")
      .as_raw(),
    gpu.scratch.as_raw()
  );
  shape.x = 7;
  compositor.compose(&gpu, &shape).expect("same shape moved");
  assert_eq!(state.creates.load(Ordering::Acquire), before + 1);
  shape.kind = 1;
  shape.width = 16;
  shape.height = 18;
  shape.pitch = 2;
  shape.bytes = vec![0; 36];
  shape.generation += 1;
  compositor
    .compose(&gpu, &shape)
    .expect("monochrome AND/XOR metadata");
  shape.kind = 4;
  shape.height = 9;
  shape.pitch = 64;
  shape.bytes = vec![128; 64 * 9];
  shape.generation += 1;
  compositor
    .compose(&gpu, &shape)
    .expect("masked color metadata");
  assert_eq!(state.creates.load(Ordering::Acquire), before + 3);
  let constants = state.constants.lock().expect("constant data");
  assert_eq!(&constants[0][..6], &[(-3i32) as u32, 4, 17, 9, 2, 68]);
  assert_eq!(&constants[2][2..6], &[16, 9, 1, 2]);
  assert_eq!(
    state.dispatches.lock().expect("dispatches").as_slice(),
    &[(3, 2, 1), (3, 2, 1), (2, 2, 1), (2, 2, 1)]
  );
  let calls = state.calls.lock().expect("calls");
  for (index, call) in calls.iter().enumerate() {
    if *call == "Dispatch" {
      assert_eq!(
        &calls[index + 1..index + 4],
        &["CSResourcesClear", "CSTargetClear", "CSShaderClear"]
      );
    }
  }
  drop(calls);
  drop(constants);
  shape.visible = false;
  assert_eq!(
    compositor
      .compose(&gpu, &shape)
      .expect("hidden pointer")
      .as_raw(),
    gpu.desktop.as_raw()
  );
  assert!(state.unexpected.lock().expect("unexpected").is_empty());
  drop(compositor);
  drop(gpu);
  assert_eq!(state.live.load(Ordering::Acquire), 0);
}
#[test]
fn failed_pointer_resources_preserve_native_failure_and_release_partial_views() {
  for call in [
    "CreateComputeShader",
    "CreateBuffer",
    "CreateShaderResourceView",
    "CreateUnorderedAccessView",
  ] {
    let (gpu, state) = fixture();
    *state.failure.lock().expect("failure") = Some(call);
    let error = super::super::super::cursor::CursorCompositor::new(&gpu)
      .err()
      .expect("resource failure");
    assert_eq!(error.native_code, Some(E_FAIL.0));
    drop(gpu);
    assert_eq!(state.live.load(Ordering::Acquire), 0);
  }
}
