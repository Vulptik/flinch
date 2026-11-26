//! COM driver doubles: no real D3D device, display source or GPU allocation.
#![allow(clippy::expect_used)]
use super::*;
use windows::Win32::{
  Foundation::{E_FAIL, E_NOTIMPL},
  Graphics::Direct3D10::*,
};

use std::sync::{
  Arc, Mutex,
  atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};
use windows_core::implement;
#[path = "mock_cursor.rs"]
#[cfg(test)]
mod cursor;
use cursor::*;
pub(crate) struct State {
  pub calls: Mutex<Vec<&'static str>>,
  pub unexpected: Mutex<Vec<&'static str>>,
  pub protected: AtomicBool,
  pub reject_protection: AtomicBool,
  pub formats: AtomicBool,
  pub creates: AtomicUsize,
  pub fail_at: AtomicUsize,
  pub live: AtomicUsize,
  pub ready: AtomicBool,
  pub ticks: AtomicU64,
  pub failure: Mutex<Option<&'static str>>,
  pub dispatches: Mutex<Vec<(u32, u32, u32)>>,
  pub constants: Mutex<Vec<[u32; 8]>>,
  /// (width, height, format) of every created texture, in order.
  pub textures: Mutex<Vec<(u32, u32, DXGI_FORMAT)>>,
  /// Video processor rectangles: (setter, [left, top, right, bottom]).
  pub rects: Mutex<Vec<(&'static str, [i32; 4])>>,
}
impl State {
  fn call(&self, name: &'static str) -> windows::core::Result<()> {
    self.calls.lock().expect("calls").push(name);
    if *self.failure.lock().expect("failure") == Some(name) {
      Err(E_FAIL.into())
    } else {
      Ok(())
    }
  }
  fn unexpected(&self, name: &'static str) {
    self.unexpected.lock().expect("unexpected calls").push(name);
  }
}
#[implement(ID3D11Device, ID3D11VideoDevice)]
struct Device {
  pub state: Arc<State>,
}
impl ID3D11Device_Impl for Device_Impl {
  fn CreateBuffer(
    &self,
    _pdesc: *const D3D11_BUFFER_DESC,
    _pinitialdata: *const D3D11_SUBRESOURCE_DATA,
    _ppbuffer: windows_core::OutRef<ID3D11Buffer>,
  ) -> windows_core::Result<()> {
    self.state.call("CreateBuffer")?;
    let desc = unsafe { *_pdesc };
    assert_eq!(desc.ByteWidth, 32);
    assert_eq!(desc.BindFlags, D3D11_BIND_CONSTANT_BUFFER.0 as u32);
    _ppbuffer.write(Some(
      CursorBuffer {
        state: self.state.clone(),
        desc,
      }
      .into(),
    ))
  }
  fn CreateTexture1D(
    &self,
    _pdesc: *const D3D11_TEXTURE1D_DESC,
    _pinitialdata: *const D3D11_SUBRESOURCE_DATA,
    _pptexture1d: windows_core::OutRef<ID3D11Texture1D>,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Device.CreateTexture1D");
    Err(E_NOTIMPL.into())
  }
  fn CreateTexture2D(
    &self,
    _pdesc: *const D3D11_TEXTURE2D_DESC,
    _pinitialdata: *const D3D11_SUBRESOURCE_DATA,
    _pptexture2d: windows_core::OutRef<ID3D11Texture2D>,
  ) -> windows_core::Result<()> {
    self.state.call("CreateTexture2D")?;
    let index = self.state.creates.fetch_add(1, Ordering::AcqRel) + 1;
    if self.state.fail_at.load(Ordering::Acquire) == index {
      return Err(E_FAIL.into());
    }
    let desc = unsafe { *_pdesc };
    if desc.CPUAccessFlags != 0 || desc.Usage == D3D11_USAGE_STAGING {
      return Err(E_FAIL.into());
    }
    self
      .state
      .textures
      .lock()
      .expect("textures")
      .push((desc.Width, desc.Height, desc.Format));
    self.state.live.fetch_add(1, Ordering::AcqRel);
    _pptexture2d.write(Some(
      Texture {
        state: self.state.clone(),
        desc,
      }
      .into(),
    ))
  }
  fn CreateTexture3D(
    &self,
    _pdesc: *const D3D11_TEXTURE3D_DESC,
    _pinitialdata: *const D3D11_SUBRESOURCE_DATA,
    _pptexture3d: windows_core::OutRef<ID3D11Texture3D>,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Device.CreateTexture3D");
    Err(E_NOTIMPL.into())
  }
  fn CreateShaderResourceView(
    &self,
    _presource: windows_core::Ref<ID3D11Resource>,
    _pdesc: *const D3D11_SHADER_RESOURCE_VIEW_DESC,
    _ppsrview: windows_core::OutRef<ID3D11ShaderResourceView>,
  ) -> windows_core::Result<()> {
    self.state.call("CreateShaderResourceView")?;
    _ppsrview.write(Some(
      CursorView {
        state: self.state.clone(),
        resource: _presource.ok()?.clone(),
      }
      .into(),
    ))
  }
  fn CreateUnorderedAccessView(
    &self,
    _presource: windows_core::Ref<ID3D11Resource>,
    _pdesc: *const D3D11_UNORDERED_ACCESS_VIEW_DESC,
    _ppuaview: windows_core::OutRef<ID3D11UnorderedAccessView>,
  ) -> windows_core::Result<()> {
    self.state.call("CreateUnorderedAccessView")?;
    _ppuaview.write(Some(
      CursorView {
        state: self.state.clone(),
        resource: _presource.ok()?.clone(),
      }
      .into(),
    ))
  }
  fn CreateRenderTargetView(
    &self,
    _presource: windows_core::Ref<ID3D11Resource>,
    _pdesc: *const D3D11_RENDER_TARGET_VIEW_DESC,
    _pprtview: windows_core::OutRef<ID3D11RenderTargetView>,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Device.CreateRenderTargetView");
    Err(E_NOTIMPL.into())
  }
  fn CreateDepthStencilView(
    &self,
    _presource: windows_core::Ref<ID3D11Resource>,
    _pdesc: *const D3D11_DEPTH_STENCIL_VIEW_DESC,
    _ppdepthstencilview: windows_core::OutRef<ID3D11DepthStencilView>,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Device.CreateDepthStencilView");
    Err(E_NOTIMPL.into())
  }
  fn CreateInputLayout(
    &self,
    _pinputelementdescs: *const D3D11_INPUT_ELEMENT_DESC,
    _numelements: u32,
    _pshaderbytecodewithinputsignature: *const core::ffi::c_void,
    _bytecodelength: usize,
    _ppinputlayout: windows_core::OutRef<ID3D11InputLayout>,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Device.CreateInputLayout");
    Err(E_NOTIMPL.into())
  }
  fn CreateVertexShader(
    &self,
    _pshaderbytecode: *const core::ffi::c_void,
    _bytecodelength: usize,
    _pclasslinkage: windows_core::Ref<ID3D11ClassLinkage>,
    _ppvertexshader: windows_core::OutRef<ID3D11VertexShader>,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Device.CreateVertexShader");
    Err(E_NOTIMPL.into())
  }
  fn CreateGeometryShader(
    &self,
    _pshaderbytecode: *const core::ffi::c_void,
    _bytecodelength: usize,
    _pclasslinkage: windows_core::Ref<ID3D11ClassLinkage>,
    _ppgeometryshader: windows_core::OutRef<ID3D11GeometryShader>,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Device.CreateGeometryShader");
    Err(E_NOTIMPL.into())
  }
  fn CreateGeometryShaderWithStreamOutput(
    &self,
    _pshaderbytecode: *const core::ffi::c_void,
    _bytecodelength: usize,
    _psodeclaration: *const D3D11_SO_DECLARATION_ENTRY,
    _numentries: u32,
    _pbufferstrides: *const u32,
    _numstrides: u32,
    _rasterizedstream: u32,
    _pclasslinkage: windows_core::Ref<ID3D11ClassLinkage>,
    _ppgeometryshader: windows_core::OutRef<ID3D11GeometryShader>,
  ) -> windows_core::Result<()> {
    self
      .state
      .unexpected("Device.CreateGeometryShaderWithStreamOutput");
    Err(E_NOTIMPL.into())
  }
  fn CreatePixelShader(
    &self,
    _pshaderbytecode: *const core::ffi::c_void,
    _bytecodelength: usize,
    _pclasslinkage: windows_core::Ref<ID3D11ClassLinkage>,
    _pppixelshader: windows_core::OutRef<ID3D11PixelShader>,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Device.CreatePixelShader");
    Err(E_NOTIMPL.into())
  }
  fn CreateHullShader(
    &self,
    _pshaderbytecode: *const core::ffi::c_void,
    _bytecodelength: usize,
    _pclasslinkage: windows_core::Ref<ID3D11ClassLinkage>,
    _pphullshader: windows_core::OutRef<ID3D11HullShader>,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Device.CreateHullShader");
    Err(E_NOTIMPL.into())
  }
  fn CreateDomainShader(
    &self,
    _pshaderbytecode: *const core::ffi::c_void,
    _bytecodelength: usize,
    _pclasslinkage: windows_core::Ref<ID3D11ClassLinkage>,
    _ppdomainshader: windows_core::OutRef<ID3D11DomainShader>,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Device.CreateDomainShader");
    Err(E_NOTIMPL.into())
  }
  fn CreateComputeShader(
    &self,
    _pshaderbytecode: *const core::ffi::c_void,
    _bytecodelength: usize,
    _pclasslinkage: windows_core::Ref<ID3D11ClassLinkage>,
    _ppcomputeshader: windows_core::OutRef<ID3D11ComputeShader>,
  ) -> windows_core::Result<()> {
    self.state.call("CreateComputeShader")?;
    assert!(!_pshaderbytecode.is_null() && _bytecodelength > 0);
    _ppcomputeshader.write(Some(
      CursorShader {
        state: self.state.clone(),
      }
      .into(),
    ))
  }
  fn CreateClassLinkage(&self) -> windows_core::Result<ID3D11ClassLinkage> {
    self.state.unexpected("Device.CreateClassLinkage");
    Err(E_NOTIMPL.into())
  }
  fn CreateBlendState(
    &self,
    _pblendstatedesc: *const D3D11_BLEND_DESC,
    _ppblendstate: windows_core::OutRef<ID3D11BlendState>,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Device.CreateBlendState");
    Err(E_NOTIMPL.into())
  }
  fn CreateDepthStencilState(
    &self,
    _pdepthstencildesc: *const D3D11_DEPTH_STENCIL_DESC,
    _ppdepthstencilstate: windows_core::OutRef<ID3D11DepthStencilState>,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Device.CreateDepthStencilState");
    Err(E_NOTIMPL.into())
  }
  fn CreateRasterizerState(
    &self,
    _prasterizerdesc: *const D3D11_RASTERIZER_DESC,
    _pprasterizerstate: windows_core::OutRef<ID3D11RasterizerState>,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Device.CreateRasterizerState");
    Err(E_NOTIMPL.into())
  }
  fn CreateSamplerState(
    &self,
    _psamplerdesc: *const D3D11_SAMPLER_DESC,
    _ppsamplerstate: windows_core::OutRef<ID3D11SamplerState>,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Device.CreateSamplerState");
    Err(E_NOTIMPL.into())
  }
  fn CreateQuery(
    &self,
    _pquerydesc: *const D3D11_QUERY_DESC,
    _ppquery: windows_core::OutRef<ID3D11Query>,
  ) -> windows_core::Result<()> {
    self.state.call("CreateQuery")?;
    _ppquery.write(Some(
      Query {
        state: self.state.clone(),
        desc: unsafe { *_pquerydesc },
      }
      .into(),
    ))
  }
  fn CreatePredicate(
    &self,
    _ppredicatedesc: *const D3D11_QUERY_DESC,
    _pppredicate: windows_core::OutRef<ID3D11Predicate>,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Device.CreatePredicate");
    Err(E_NOTIMPL.into())
  }
  fn CreateCounter(
    &self,
    _pcounterdesc: *const D3D11_COUNTER_DESC,
    _ppcounter: windows_core::OutRef<ID3D11Counter>,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Device.CreateCounter");
    Err(E_NOTIMPL.into())
  }
  fn CreateDeferredContext(
    &self,
    _contextflags: u32,
    _ppdeferredcontext: windows_core::OutRef<ID3D11DeviceContext>,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Device.CreateDeferredContext");
    Err(E_NOTIMPL.into())
  }
  fn OpenSharedResource(
    &self,
    _hresource: windows::Win32::Foundation::HANDLE,
    _returnedinterface: *const windows_core::GUID,
    _ppresource: *mut *mut core::ffi::c_void,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Device.OpenSharedResource");
    Err(E_NOTIMPL.into())
  }
  fn CheckFormatSupport(
    &self,
    _format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
  ) -> windows_core::Result<u32> {
    self.state.unexpected("Device.CheckFormatSupport");
    Err(E_NOTIMPL.into())
  }
  fn CheckMultisampleQualityLevels(
    &self,
    _format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
    _samplecount: u32,
  ) -> windows_core::Result<u32> {
    self
      .state
      .unexpected("Device.CheckMultisampleQualityLevels");
    Err(E_NOTIMPL.into())
  }
  fn CheckCounterInfo(&self, _pcounterinfo: *mut D3D11_COUNTER_INFO) {
    self.state.unexpected("Device.CheckCounterInfo");
  }
  fn CheckCounter(
    &self,
    _pdesc: *const D3D11_COUNTER_DESC,
    _ptype: *mut D3D11_COUNTER_TYPE,
    _pactivecounters: *mut u32,
    _szname: windows_core::PSTR,
    _pnamelength: *mut u32,
    _szunits: windows_core::PSTR,
    _punitslength: *mut u32,
    _szdescription: windows_core::PSTR,
    _pdescriptionlength: *mut u32,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Device.CheckCounter");
    Err(E_NOTIMPL.into())
  }
  fn CheckFeatureSupport(
    &self,
    _feature: D3D11_FEATURE,
    _pfeaturesupportdata: *mut core::ffi::c_void,
    _featuresupportdatasize: u32,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Device.CheckFeatureSupport");
    Err(E_NOTIMPL.into())
  }
  fn GetPrivateData(
    &self,
    _guid: *const windows_core::GUID,
    _pdatasize: *mut u32,
    _pdata: *mut core::ffi::c_void,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Device.GetPrivateData");
    Err(E_NOTIMPL.into())
  }
  fn SetPrivateData(
    &self,
    _guid: *const windows_core::GUID,
    _datasize: u32,
    _pdata: *const core::ffi::c_void,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Device.SetPrivateData");
    Err(E_NOTIMPL.into())
  }
  fn SetPrivateDataInterface(
    &self,
    _guid: *const windows_core::GUID,
    _pdata: windows_core::Ref<windows_core::IUnknown>,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Device.SetPrivateDataInterface");
    Err(E_NOTIMPL.into())
  }
  fn GetFeatureLevel(&self) -> windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL {
    self.state.unexpected("Device.GetFeatureLevel");
    Default::default()
  }
  fn GetCreationFlags(&self) -> u32 {
    self.state.unexpected("Device.GetCreationFlags");
    Default::default()
  }
  fn GetDeviceRemovedReason(&self) -> windows_core::Result<()> {
    self.state.unexpected("Device.GetDeviceRemovedReason");
    Err(E_NOTIMPL.into())
  }
  fn GetImmediateContext(&self, _ppimmediatecontext: windows_core::OutRef<ID3D11DeviceContext>) {
    self.state.unexpected("Device.GetImmediateContext");
  }
  fn SetExceptionMode(&self, _raiseflags: u32) -> windows_core::Result<()> {
    self.state.unexpected("Device.SetExceptionMode");
    Err(E_NOTIMPL.into())
  }
  fn GetExceptionMode(&self) -> u32 {
    self.state.unexpected("Device.GetExceptionMode");
    Default::default()
  }
}
impl ID3D11VideoDevice_Impl for Device_Impl {
  fn CreateVideoDecoder(
    &self,
    _pvideodesc: *const D3D11_VIDEO_DECODER_DESC,
    _pconfig: *const D3D11_VIDEO_DECODER_CONFIG,
  ) -> windows_core::Result<ID3D11VideoDecoder> {
    self.state.unexpected("Device.CreateVideoDecoder");
    Err(E_NOTIMPL.into())
  }
  fn CreateVideoProcessor(
    &self,
    _penum: windows_core::Ref<ID3D11VideoProcessorEnumerator>,
    _rateconversionindex: u32,
  ) -> windows_core::Result<ID3D11VideoProcessor> {
    self.state.call("CreateVideoProcessor")?;
    Ok(
      Processor {
        state: self.state.clone(),
      }
      .into(),
    )
  }
  fn CreateAuthenticatedChannel(
    &self,
    _channeltype: D3D11_AUTHENTICATED_CHANNEL_TYPE,
  ) -> windows_core::Result<ID3D11AuthenticatedChannel> {
    self.state.unexpected("Device.CreateAuthenticatedChannel");
    Err(E_NOTIMPL.into())
  }
  fn CreateCryptoSession(
    &self,
    _pcryptotype: *const windows_core::GUID,
    _pdecoderprofile: *const windows_core::GUID,
    _pkeyexchangetype: *const windows_core::GUID,
  ) -> windows_core::Result<ID3D11CryptoSession> {
    self.state.unexpected("Device.CreateCryptoSession");
    Err(E_NOTIMPL.into())
  }
  fn CreateVideoDecoderOutputView(
    &self,
    _presource: windows_core::Ref<ID3D11Resource>,
    _pdesc: *const D3D11_VIDEO_DECODER_OUTPUT_VIEW_DESC,
    _ppvdovview: windows_core::OutRef<ID3D11VideoDecoderOutputView>,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Device.CreateVideoDecoderOutputView");
    Err(E_NOTIMPL.into())
  }
  fn CreateVideoProcessorInputView(
    &self,
    _presource: windows_core::Ref<ID3D11Resource>,
    _penum: windows_core::Ref<ID3D11VideoProcessorEnumerator>,
    _pdesc: *const D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
    _ppvpiview: windows_core::OutRef<ID3D11VideoProcessorInputView>,
  ) -> windows_core::Result<()> {
    self.state.call("CreateVideoProcessorInputView")?;
    _ppvpiview.write(Some(
      InputView {
        state: self.state.clone(),
        resource: _presource.ok()?.clone(),
      }
      .into(),
    ))
  }
  fn CreateVideoProcessorOutputView(
    &self,
    _presource: windows_core::Ref<ID3D11Resource>,
    _penum: windows_core::Ref<ID3D11VideoProcessorEnumerator>,
    _pdesc: *const D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    _ppvpoview: windows_core::OutRef<ID3D11VideoProcessorOutputView>,
  ) -> windows_core::Result<()> {
    self.state.call("CreateVideoProcessorOutputView")?;
    _ppvpoview.write(Some(
      OutputView {
        state: self.state.clone(),
        resource: _presource.ok()?.clone(),
      }
      .into(),
    ))
  }
  fn CreateVideoProcessorEnumerator(
    &self,
    _pdesc: *const D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
  ) -> windows_core::Result<ID3D11VideoProcessorEnumerator> {
    self.state.call("CreateVideoProcessorEnumerator")?;
    let desc = unsafe { &*_pdesc };
    // A crop shrinks the output; this pipeline never upscales.
    if desc.OutputWidth > desc.InputWidth || desc.OutputHeight > desc.InputHeight {
      return Err(E_FAIL.into());
    }
    Ok(
      Enumerator {
        state: self.state.clone(),
      }
      .into(),
    )
  }
  fn GetVideoDecoderProfileCount(&self) -> u32 {
    self.state.unexpected("Device.GetVideoDecoderProfileCount");
    Default::default()
  }
  fn GetVideoDecoderProfile(&self, _index: u32) -> windows_core::Result<windows_core::GUID> {
    self.state.unexpected("Device.GetVideoDecoderProfile");
    Err(E_NOTIMPL.into())
  }
  fn CheckVideoDecoderFormat(
    &self,
    _pdecoderprofile: *const windows_core::GUID,
    _format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
  ) -> windows_core::Result<windows_core::BOOL> {
    self.state.unexpected("Device.CheckVideoDecoderFormat");
    Err(E_NOTIMPL.into())
  }
  fn GetVideoDecoderConfigCount(
    &self,
    _pdesc: *const D3D11_VIDEO_DECODER_DESC,
  ) -> windows_core::Result<u32> {
    self.state.unexpected("Device.GetVideoDecoderConfigCount");
    Err(E_NOTIMPL.into())
  }
  fn GetVideoDecoderConfig(
    &self,
    _pdesc: *const D3D11_VIDEO_DECODER_DESC,
    _index: u32,
    _pconfig: *mut D3D11_VIDEO_DECODER_CONFIG,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Device.GetVideoDecoderConfig");
    Err(E_NOTIMPL.into())
  }
  fn GetContentProtectionCaps(
    &self,
    _pcryptotype: *const windows_core::GUID,
    _pdecoderprofile: *const windows_core::GUID,
    _pcaps: *mut D3D11_VIDEO_CONTENT_PROTECTION_CAPS,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Device.GetContentProtectionCaps");
    Err(E_NOTIMPL.into())
  }
  fn CheckCryptoKeyExchange(
    &self,
    _pcryptotype: *const windows_core::GUID,
    _pdecoderprofile: *const windows_core::GUID,
    _index: u32,
  ) -> windows_core::Result<windows_core::GUID> {
    self.state.unexpected("Device.CheckCryptoKeyExchange");
    Err(E_NOTIMPL.into())
  }
  fn SetPrivateData(
    &self,
    _guid: *const windows_core::GUID,
    _datasize: u32,
    _pdata: *const core::ffi::c_void,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Device.SetPrivateData");
    Err(E_NOTIMPL.into())
  }
  fn SetPrivateDataInterface(
    &self,
    _guid: *const windows_core::GUID,
    _pdata: windows_core::Ref<windows_core::IUnknown>,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Device.SetPrivateDataInterface");
    Err(E_NOTIMPL.into())
  }
}
#[implement(ID3D11DeviceContext1, ID3D11VideoContext1, ID3D10Multithread)]
struct Context {
  pub state: Arc<State>,
}
impl ID3D11DeviceChild_Impl for Context_Impl {
  fn GetDevice(&self, _ppdevice: windows_core::OutRef<ID3D11Device>) {
    self.state.unexpected("Context.GetDevice");
  }
  fn GetPrivateData(
    &self,
    _guid: *const windows_core::GUID,
    _pdatasize: *mut u32,
    _pdata: *mut core::ffi::c_void,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Context.GetPrivateData");
    Err(E_NOTIMPL.into())
  }
  fn SetPrivateData(
    &self,
    _guid: *const windows_core::GUID,
    _datasize: u32,
    _pdata: *const core::ffi::c_void,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Context.SetPrivateData");
    Err(E_NOTIMPL.into())
  }
  fn SetPrivateDataInterface(
    &self,
    _guid: *const windows_core::GUID,
    _pdata: windows_core::Ref<windows_core::IUnknown>,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Context.SetPrivateDataInterface");
    Err(E_NOTIMPL.into())
  }
}
impl ID3D11DeviceContext_Impl for Context_Impl {
  fn VSSetConstantBuffers(
    &self,
    _startslot: u32,
    _numbuffers: u32,
    _ppconstantbuffers: *const Option<ID3D11Buffer>,
  ) {
    self.state.unexpected("Context.VSSetConstantBuffers");
  }
  fn PSSetShaderResources(
    &self,
    _startslot: u32,
    _numviews: u32,
    _ppshaderresourceviews: *const Option<ID3D11ShaderResourceView>,
  ) {
    self.state.unexpected("Context.PSSetShaderResources");
  }
  fn PSSetShader(
    &self,
    _ppixelshader: windows_core::Ref<ID3D11PixelShader>,
    _ppclassinstances: *const Option<ID3D11ClassInstance>,
    _numclassinstances: u32,
  ) {
    self.state.unexpected("Context.PSSetShader");
  }
  fn PSSetSamplers(
    &self,
    _startslot: u32,
    _numsamplers: u32,
    _ppsamplers: *const Option<ID3D11SamplerState>,
  ) {
    self.state.unexpected("Context.PSSetSamplers");
  }
  fn VSSetShader(
    &self,
    _pvertexshader: windows_core::Ref<ID3D11VertexShader>,
    _ppclassinstances: *const Option<ID3D11ClassInstance>,
    _numclassinstances: u32,
  ) {
    self.state.unexpected("Context.VSSetShader");
  }
  fn DrawIndexed(&self, _indexcount: u32, _startindexlocation: u32, _basevertexlocation: i32) {
    self.state.unexpected("Context.DrawIndexed");
  }
  fn Draw(&self, _vertexcount: u32, _startvertexlocation: u32) {
    self.state.unexpected("Context.Draw");
  }
  fn Map(
    &self,
    _presource: windows_core::Ref<ID3D11Resource>,
    _subresource: u32,
    _maptype: D3D11_MAP,
    _mapflags: u32,
    _pmappedresource: *mut D3D11_MAPPED_SUBRESOURCE,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Context.Map");
    Err(E_NOTIMPL.into())
  }
  fn Unmap(&self, _presource: windows_core::Ref<ID3D11Resource>, _subresource: u32) {
    self.state.unexpected("Context.Unmap");
  }
  fn PSSetConstantBuffers(
    &self,
    _startslot: u32,
    _numbuffers: u32,
    _ppconstantbuffers: *const Option<ID3D11Buffer>,
  ) {
    self.state.unexpected("Context.PSSetConstantBuffers");
  }
  fn IASetInputLayout(&self, _pinputlayout: windows_core::Ref<ID3D11InputLayout>) {
    self.state.unexpected("Context.IASetInputLayout");
  }
  fn IASetVertexBuffers(
    &self,
    _startslot: u32,
    _numbuffers: u32,
    _ppvertexbuffers: *const Option<ID3D11Buffer>,
    _pstrides: *const u32,
    _poffsets: *const u32,
  ) {
    self.state.unexpected("Context.IASetVertexBuffers");
  }
  fn IASetIndexBuffer(
    &self,
    _pindexbuffer: windows_core::Ref<ID3D11Buffer>,
    _format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
    _offset: u32,
  ) {
    self.state.unexpected("Context.IASetIndexBuffer");
  }
  fn DrawIndexedInstanced(
    &self,
    _indexcountperinstance: u32,
    _instancecount: u32,
    _startindexlocation: u32,
    _basevertexlocation: i32,
    _startinstancelocation: u32,
  ) {
    self.state.unexpected("Context.DrawIndexedInstanced");
  }
  fn DrawInstanced(
    &self,
    _vertexcountperinstance: u32,
    _instancecount: u32,
    _startvertexlocation: u32,
    _startinstancelocation: u32,
  ) {
    self.state.unexpected("Context.DrawInstanced");
  }
  fn GSSetConstantBuffers(
    &self,
    _startslot: u32,
    _numbuffers: u32,
    _ppconstantbuffers: *const Option<ID3D11Buffer>,
  ) {
    self.state.unexpected("Context.GSSetConstantBuffers");
  }
  fn GSSetShader(
    &self,
    _pshader: windows_core::Ref<ID3D11GeometryShader>,
    _ppclassinstances: *const Option<ID3D11ClassInstance>,
    _numclassinstances: u32,
  ) {
    self.state.unexpected("Context.GSSetShader");
  }
  fn IASetPrimitiveTopology(
    &self,
    _topology: windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY,
  ) {
    self.state.unexpected("Context.IASetPrimitiveTopology");
  }
  fn VSSetShaderResources(
    &self,
    _startslot: u32,
    _numviews: u32,
    _ppshaderresourceviews: *const Option<ID3D11ShaderResourceView>,
  ) {
    self.state.unexpected("Context.VSSetShaderResources");
  }
  fn VSSetSamplers(
    &self,
    _startslot: u32,
    _numsamplers: u32,
    _ppsamplers: *const Option<ID3D11SamplerState>,
  ) {
    self.state.unexpected("Context.VSSetSamplers");
  }
  fn Begin(&self, _pasync: windows_core::Ref<ID3D11Asynchronous>) {
    self.state.calls.lock().expect("calls").push("Begin");
  }
  fn End(&self, _pasync: windows_core::Ref<ID3D11Asynchronous>) {
    self.state.calls.lock().expect("calls").push("End");
  }
  fn GetData(
    &self,
    _pasync: windows_core::Ref<ID3D11Asynchronous>,
    _pdata: *mut core::ffi::c_void,
    _datasize: u32,
    _getdataflags: u32,
  ) -> windows_core::Result<()> {
    self.state.call("GetData")?;
    if _getdataflags != D3D11_ASYNC_GETDATA_DONOTFLUSH.0 as u32 {
      return Err(E_FAIL.into());
    }
    if !self.state.ready.load(Ordering::Acquire) {
      return Ok(());
    }
    if _datasize == 8 {
      unsafe {
        *_pdata.cast::<u64>() = self.state.ticks.fetch_add(100, Ordering::AcqRel);
      }
    } else if _datasize == std::mem::size_of::<D3D11_QUERY_DATA_TIMESTAMP_DISJOINT>() as u32 {
      unsafe {
        *_pdata.cast::<D3D11_QUERY_DATA_TIMESTAMP_DISJOINT>() =
          D3D11_QUERY_DATA_TIMESTAMP_DISJOINT {
            Frequency: 1_000_000,
            Disjoint: false.into(),
          };
      }
    } else {
      return Err(E_FAIL.into());
    }
    Ok(())
  }
  fn SetPredication(
    &self,
    _ppredicate: windows_core::Ref<ID3D11Predicate>,
    _predicatevalue: windows_core::BOOL,
  ) {
    self.state.unexpected("Context.SetPredication");
  }
  fn GSSetShaderResources(
    &self,
    _startslot: u32,
    _numviews: u32,
    _ppshaderresourceviews: *const Option<ID3D11ShaderResourceView>,
  ) {
    self.state.unexpected("Context.GSSetShaderResources");
  }
  fn GSSetSamplers(
    &self,
    _startslot: u32,
    _numsamplers: u32,
    _ppsamplers: *const Option<ID3D11SamplerState>,
  ) {
    self.state.unexpected("Context.GSSetSamplers");
  }
  fn OMSetRenderTargets(
    &self,
    _numviews: u32,
    _pprendertargetviews: *const Option<ID3D11RenderTargetView>,
    _pdepthstencilview: windows_core::Ref<ID3D11DepthStencilView>,
  ) {
    self.state.unexpected("Context.OMSetRenderTargets");
  }
  fn OMSetRenderTargetsAndUnorderedAccessViews(
    &self,
    _numrtvs: u32,
    _pprendertargetviews: *const Option<ID3D11RenderTargetView>,
    _pdepthstencilview: windows_core::Ref<ID3D11DepthStencilView>,
    _uavstartslot: u32,
    _numuavs: u32,
    _ppunorderedaccessviews: *const Option<ID3D11UnorderedAccessView>,
    _puavinitialcounts: *const u32,
  ) {
    self
      .state
      .unexpected("Context.OMSetRenderTargetsAndUnorderedAccessViews");
  }
  fn OMSetBlendState(
    &self,
    _pblendstate: windows_core::Ref<ID3D11BlendState>,
    _blendfactor: *const f32,
    _samplemask: u32,
  ) {
    self.state.unexpected("Context.OMSetBlendState");
  }
  fn OMSetDepthStencilState(
    &self,
    _pdepthstencilstate: windows_core::Ref<ID3D11DepthStencilState>,
    _stencilref: u32,
  ) {
    self.state.unexpected("Context.OMSetDepthStencilState");
  }
  fn SOSetTargets(
    &self,
    _numbuffers: u32,
    _ppsotargets: *const Option<ID3D11Buffer>,
    _poffsets: *const u32,
  ) {
    self.state.unexpected("Context.SOSetTargets");
  }
  fn DrawAuto(&self) {
    self.state.unexpected("Context.DrawAuto");
  }
  fn DrawIndexedInstancedIndirect(
    &self,
    _pbufferforargs: windows_core::Ref<ID3D11Buffer>,
    _alignedbyteoffsetforargs: u32,
  ) {
    self
      .state
      .unexpected("Context.DrawIndexedInstancedIndirect");
  }
  fn DrawInstancedIndirect(
    &self,
    _pbufferforargs: windows_core::Ref<ID3D11Buffer>,
    _alignedbyteoffsetforargs: u32,
  ) {
    self.state.unexpected("Context.DrawInstancedIndirect");
  }
  fn Dispatch(&self, _threadgroupcountx: u32, _threadgroupcounty: u32, _threadgroupcountz: u32) {
    self.state.dispatches.lock().expect("dispatch").push((
      _threadgroupcountx,
      _threadgroupcounty,
      _threadgroupcountz,
    ));
    self.state.calls.lock().expect("calls").push("Dispatch");
  }
  fn DispatchIndirect(
    &self,
    _pbufferforargs: windows_core::Ref<ID3D11Buffer>,
    _alignedbyteoffsetforargs: u32,
  ) {
    self.state.unexpected("Context.DispatchIndirect");
  }
  fn RSSetState(&self, _prasterizerstate: windows_core::Ref<ID3D11RasterizerState>) {
    self.state.unexpected("Context.RSSetState");
  }
  fn RSSetViewports(&self, _numviewports: u32, _pviewports: *const D3D11_VIEWPORT) {
    self.state.unexpected("Context.RSSetViewports");
  }
  fn RSSetScissorRects(&self, _numrects: u32, _prects: *const windows::Win32::Foundation::RECT) {
    self.state.unexpected("Context.RSSetScissorRects");
  }
  fn CopySubresourceRegion(
    &self,
    _pdstresource: windows_core::Ref<ID3D11Resource>,
    _dstsubresource: u32,
    _dstx: u32,
    _dsty: u32,
    _dstz: u32,
    _psrcresource: windows_core::Ref<ID3D11Resource>,
    _srcsubresource: u32,
    _psrcbox: *const D3D11_BOX,
  ) {
    self.state.unexpected("Context.CopySubresourceRegion");
  }
  fn CopyResource(
    &self,
    _pdstresource: windows_core::Ref<ID3D11Resource>,
    _psrcresource: windows_core::Ref<ID3D11Resource>,
  ) {
    self.state.calls.lock().expect("calls").push("CopyResource");
  }

  fn UpdateSubresource(
    &self,
    _pdstresource: windows_core::Ref<ID3D11Resource>,
    _dstsubresource: u32,
    _pdstbox: *const D3D11_BOX,
    _psrcdata: *const core::ffi::c_void,
    _srcrowpitch: u32,
    _srcdepthpitch: u32,
  ) {
    assert!(!_psrcdata.is_null());
    self
      .state
      .constants
      .lock()
      .expect("constants")
      .push(unsafe { _psrcdata.cast::<[u32; 8]>().read_unaligned() });
  }
  fn CopyStructureCount(
    &self,
    _pdstbuffer: windows_core::Ref<ID3D11Buffer>,
    _dstalignedbyteoffset: u32,
    _psrcview: windows_core::Ref<ID3D11UnorderedAccessView>,
  ) {
    self.state.unexpected("Context.CopyStructureCount");
  }
  fn ClearRenderTargetView(
    &self,
    _prendertargetview: windows_core::Ref<ID3D11RenderTargetView>,
    _colorrgba: *const f32,
  ) {
    self.state.unexpected("Context.ClearRenderTargetView");
  }
  fn ClearUnorderedAccessViewUint(
    &self,
    _punorderedaccessview: windows_core::Ref<ID3D11UnorderedAccessView>,
    _values: *const u32,
  ) {
    self
      .state
      .unexpected("Context.ClearUnorderedAccessViewUint");
  }
  fn ClearUnorderedAccessViewFloat(
    &self,
    _punorderedaccessview: windows_core::Ref<ID3D11UnorderedAccessView>,
    _values: *const f32,
  ) {
    self
      .state
      .unexpected("Context.ClearUnorderedAccessViewFloat");
  }
  fn ClearDepthStencilView(
    &self,
    _pdepthstencilview: windows_core::Ref<ID3D11DepthStencilView>,
    _clearflags: u32,
    _depth: f32,
    _stencil: u8,
  ) {
    self.state.unexpected("Context.ClearDepthStencilView");
  }
  fn GenerateMips(&self, _pshaderresourceview: windows_core::Ref<ID3D11ShaderResourceView>) {
    self.state.unexpected("Context.GenerateMips");
  }
  fn SetResourceMinLOD(&self, _presource: windows_core::Ref<ID3D11Resource>, _minlod: f32) {
    self.state.unexpected("Context.SetResourceMinLOD");
  }
  fn GetResourceMinLOD(&self, _presource: windows_core::Ref<ID3D11Resource>) -> f32 {
    self.state.unexpected("Context.GetResourceMinLOD");
    Default::default()
  }
  fn ResolveSubresource(
    &self,
    _pdstresource: windows_core::Ref<ID3D11Resource>,
    _dstsubresource: u32,
    _psrcresource: windows_core::Ref<ID3D11Resource>,
    _srcsubresource: u32,
    _format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
  ) {
    self.state.unexpected("Context.ResolveSubresource");
  }
  fn ExecuteCommandList(
    &self,
    _pcommandlist: windows_core::Ref<ID3D11CommandList>,
    _restorecontextstate: windows_core::BOOL,
  ) {
    self.state.unexpected("Context.ExecuteCommandList");
  }
  fn HSSetShaderResources(
    &self,
    _startslot: u32,
    _numviews: u32,
    _ppshaderresourceviews: *const Option<ID3D11ShaderResourceView>,
  ) {
    self.state.unexpected("Context.HSSetShaderResources");
  }
  fn HSSetShader(
    &self,
    _phullshader: windows_core::Ref<ID3D11HullShader>,
    _ppclassinstances: *const Option<ID3D11ClassInstance>,
    _numclassinstances: u32,
  ) {
    self.state.unexpected("Context.HSSetShader");
  }
  fn HSSetSamplers(
    &self,
    _startslot: u32,
    _numsamplers: u32,
    _ppsamplers: *const Option<ID3D11SamplerState>,
  ) {
    self.state.unexpected("Context.HSSetSamplers");
  }
  fn HSSetConstantBuffers(
    &self,
    _startslot: u32,
    _numbuffers: u32,
    _ppconstantbuffers: *const Option<ID3D11Buffer>,
  ) {
    self.state.unexpected("Context.HSSetConstantBuffers");
  }
  fn DSSetShaderResources(
    &self,
    _startslot: u32,
    _numviews: u32,
    _ppshaderresourceviews: *const Option<ID3D11ShaderResourceView>,
  ) {
    self.state.unexpected("Context.DSSetShaderResources");
  }
  fn DSSetShader(
    &self,
    _pdomainshader: windows_core::Ref<ID3D11DomainShader>,
    _ppclassinstances: *const Option<ID3D11ClassInstance>,
    _numclassinstances: u32,
  ) {
    self.state.unexpected("Context.DSSetShader");
  }
  fn DSSetSamplers(
    &self,
    _startslot: u32,
    _numsamplers: u32,
    _ppsamplers: *const Option<ID3D11SamplerState>,
  ) {
    self.state.unexpected("Context.DSSetSamplers");
  }
  fn DSSetConstantBuffers(
    &self,
    _startslot: u32,
    _numbuffers: u32,
    _ppconstantbuffers: *const Option<ID3D11Buffer>,
  ) {
    self.state.unexpected("Context.DSSetConstantBuffers");
  }
  fn CSSetShaderResources(
    &self,
    _startslot: u32,
    _numviews: u32,
    _ppshaderresourceviews: *const Option<ID3D11ShaderResourceView>,
  ) {
    assert_eq!((_startslot, _numviews), (0, 2));
    let bound = unsafe { (*_ppshaderresourceviews).is_some() };
    self.state.calls.lock().expect("calls").push(if bound {
      "CSResourcesBind"
    } else {
      "CSResourcesClear"
    });
  }
  fn CSSetUnorderedAccessViews(
    &self,
    _startslot: u32,
    _numuavs: u32,
    _ppunorderedaccessviews: *const Option<ID3D11UnorderedAccessView>,
    _puavinitialcounts: *const u32,
  ) {
    assert_eq!((_startslot, _numuavs), (0, 1));
    let bound = unsafe { (*_ppunorderedaccessviews).is_some() };
    self.state.calls.lock().expect("calls").push(if bound {
      "CSTargetBind"
    } else {
      "CSTargetClear"
    });
  }
  fn CSSetShader(
    &self,
    _pcomputeshader: windows_core::Ref<ID3D11ComputeShader>,
    _ppclassinstances: *const Option<ID3D11ClassInstance>,
    _numclassinstances: u32,
  ) {
    assert_eq!(_numclassinstances, 0);
    self
      .state
      .calls
      .lock()
      .expect("calls")
      .push(if _pcomputeshader.is_some() {
        "CSShaderBind"
      } else {
        "CSShaderClear"
      });
  }
  fn CSSetSamplers(
    &self,
    _startslot: u32,
    _numsamplers: u32,
    _ppsamplers: *const Option<ID3D11SamplerState>,
  ) {
    self.state.unexpected("Context.CSSetSamplers");
  }
  fn CSSetConstantBuffers(
    &self,
    _startslot: u32,
    _numbuffers: u32,
    _ppconstantbuffers: *const Option<ID3D11Buffer>,
  ) {
    assert_eq!((_startslot, _numbuffers), (0, 1));
    assert!(unsafe { (*_ppconstantbuffers).is_some() });
    self.state.calls.lock().expect("calls").push("CSConstants");
  }
  fn VSGetConstantBuffers(
    &self,
    _startslot: u32,
    _numbuffers: u32,
    _ppconstantbuffers: *mut Option<ID3D11Buffer>,
  ) {
    self.state.unexpected("Context.VSGetConstantBuffers");
  }
  fn PSGetShaderResources(
    &self,
    _startslot: u32,
    _numviews: u32,
    _ppshaderresourceviews: *mut Option<ID3D11ShaderResourceView>,
  ) {
    self.state.unexpected("Context.PSGetShaderResources");
  }
  fn PSGetShader(
    &self,
    _pppixelshader: windows_core::OutRef<ID3D11PixelShader>,
    _ppclassinstances: windows_core::OutRef<ID3D11ClassInstance>,
    _pnumclassinstances: *mut u32,
  ) {
    self.state.unexpected("Context.PSGetShader");
  }
  fn PSGetSamplers(
    &self,
    _startslot: u32,
    _numsamplers: u32,
    _ppsamplers: *mut Option<ID3D11SamplerState>,
  ) {
    self.state.unexpected("Context.PSGetSamplers");
  }
  fn VSGetShader(
    &self,
    _ppvertexshader: windows_core::OutRef<ID3D11VertexShader>,
    _ppclassinstances: windows_core::OutRef<ID3D11ClassInstance>,
    _pnumclassinstances: *mut u32,
  ) {
    self.state.unexpected("Context.VSGetShader");
  }
  fn PSGetConstantBuffers(
    &self,
    _startslot: u32,
    _numbuffers: u32,
    _ppconstantbuffers: *mut Option<ID3D11Buffer>,
  ) {
    self.state.unexpected("Context.PSGetConstantBuffers");
  }
  fn IAGetInputLayout(&self, _ppinputlayout: windows_core::OutRef<ID3D11InputLayout>) {
    self.state.unexpected("Context.IAGetInputLayout");
  }
  fn IAGetVertexBuffers(
    &self,
    _startslot: u32,
    _numbuffers: u32,
    _ppvertexbuffers: windows_core::OutRef<ID3D11Buffer>,
    _pstrides: *mut u32,
    _poffsets: *mut u32,
  ) {
    self.state.unexpected("Context.IAGetVertexBuffers");
  }
  fn IAGetIndexBuffer(
    &self,
    _pindexbuffer: windows_core::OutRef<ID3D11Buffer>,
    _format: *mut windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
    _offset: *mut u32,
  ) {
    self.state.unexpected("Context.IAGetIndexBuffer");
  }
  fn GSGetConstantBuffers(
    &self,
    _startslot: u32,
    _numbuffers: u32,
    _ppconstantbuffers: *mut Option<ID3D11Buffer>,
  ) {
    self.state.unexpected("Context.GSGetConstantBuffers");
  }
  fn GSGetShader(
    &self,
    _ppgeometryshader: windows_core::OutRef<ID3D11GeometryShader>,
    _ppclassinstances: windows_core::OutRef<ID3D11ClassInstance>,
    _pnumclassinstances: *mut u32,
  ) {
    self.state.unexpected("Context.GSGetShader");
  }
  fn IAGetPrimitiveTopology(
    &self,
    _ptopology: *mut windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY,
  ) {
    self.state.unexpected("Context.IAGetPrimitiveTopology");
  }
  fn VSGetShaderResources(
    &self,
    _startslot: u32,
    _numviews: u32,
    _ppshaderresourceviews: *mut Option<ID3D11ShaderResourceView>,
  ) {
    self.state.unexpected("Context.VSGetShaderResources");
  }
  fn VSGetSamplers(
    &self,
    _startslot: u32,
    _numsamplers: u32,
    _ppsamplers: *mut Option<ID3D11SamplerState>,
  ) {
    self.state.unexpected("Context.VSGetSamplers");
  }
  fn GetPredication(
    &self,
    _pppredicate: windows_core::OutRef<ID3D11Predicate>,
    _ppredicatevalue: *mut windows_core::BOOL,
  ) {
    self.state.unexpected("Context.GetPredication");
  }
  fn GSGetShaderResources(
    &self,
    _startslot: u32,
    _numviews: u32,
    _ppshaderresourceviews: *mut Option<ID3D11ShaderResourceView>,
  ) {
    self.state.unexpected("Context.GSGetShaderResources");
  }
  fn GSGetSamplers(
    &self,
    _startslot: u32,
    _numsamplers: u32,
    _ppsamplers: *mut Option<ID3D11SamplerState>,
  ) {
    self.state.unexpected("Context.GSGetSamplers");
  }
  fn OMGetRenderTargets(
    &self,
    _numviews: u32,
    _pprendertargetviews: *mut Option<ID3D11RenderTargetView>,
    _ppdepthstencilview: windows_core::OutRef<ID3D11DepthStencilView>,
  ) {
    self.state.unexpected("Context.OMGetRenderTargets");
  }
  fn OMGetRenderTargetsAndUnorderedAccessViews(
    &self,
    _numrtvs: u32,
    _pprendertargetviews: *mut Option<ID3D11RenderTargetView>,
    _ppdepthstencilview: windows_core::OutRef<ID3D11DepthStencilView>,
    _uavstartslot: u32,
    _numuavs: u32,
    _ppunorderedaccessviews: *mut Option<ID3D11UnorderedAccessView>,
  ) {
    self
      .state
      .unexpected("Context.OMGetRenderTargetsAndUnorderedAccessViews");
  }
  fn OMGetBlendState(
    &self,
    _ppblendstate: windows_core::OutRef<ID3D11BlendState>,
    _blendfactor: *mut f32,
    _psamplemask: *mut u32,
  ) {
    self.state.unexpected("Context.OMGetBlendState");
  }
  fn OMGetDepthStencilState(
    &self,
    _ppdepthstencilstate: windows_core::OutRef<ID3D11DepthStencilState>,
    _pstencilref: *mut u32,
  ) {
    self.state.unexpected("Context.OMGetDepthStencilState");
  }
  fn SOGetTargets(&self, _numbuffers: u32, _ppsotargets: *mut Option<ID3D11Buffer>) {
    self.state.unexpected("Context.SOGetTargets");
  }
  fn RSGetState(&self, _pprasterizerstate: windows_core::OutRef<ID3D11RasterizerState>) {
    self.state.unexpected("Context.RSGetState");
  }
  fn RSGetViewports(&self, _pnumviewports: *mut u32, _pviewports: *mut D3D11_VIEWPORT) {
    self.state.unexpected("Context.RSGetViewports");
  }
  fn RSGetScissorRects(
    &self,
    _pnumrects: *mut u32,
    _prects: *mut windows::Win32::Foundation::RECT,
  ) {
    self.state.unexpected("Context.RSGetScissorRects");
  }
  fn HSGetShaderResources(
    &self,
    _startslot: u32,
    _numviews: u32,
    _ppshaderresourceviews: *mut Option<ID3D11ShaderResourceView>,
  ) {
    self.state.unexpected("Context.HSGetShaderResources");
  }
  fn HSGetShader(
    &self,
    _pphullshader: windows_core::OutRef<ID3D11HullShader>,
    _ppclassinstances: windows_core::OutRef<ID3D11ClassInstance>,
    _pnumclassinstances: *mut u32,
  ) {
    self.state.unexpected("Context.HSGetShader");
  }
  fn HSGetSamplers(
    &self,
    _startslot: u32,
    _numsamplers: u32,
    _ppsamplers: *mut Option<ID3D11SamplerState>,
  ) {
    self.state.unexpected("Context.HSGetSamplers");
  }
  fn HSGetConstantBuffers(
    &self,
    _startslot: u32,
    _numbuffers: u32,
    _ppconstantbuffers: *mut Option<ID3D11Buffer>,
  ) {
    self.state.unexpected("Context.HSGetConstantBuffers");
  }
  fn DSGetShaderResources(
    &self,
    _startslot: u32,
    _numviews: u32,
    _ppshaderresourceviews: *mut Option<ID3D11ShaderResourceView>,
  ) {
    self.state.unexpected("Context.DSGetShaderResources");
  }
  fn DSGetShader(
    &self,
    _ppdomainshader: windows_core::OutRef<ID3D11DomainShader>,
    _ppclassinstances: windows_core::OutRef<ID3D11ClassInstance>,
    _pnumclassinstances: *mut u32,
  ) {
    self.state.unexpected("Context.DSGetShader");
  }
  fn DSGetSamplers(
    &self,
    _startslot: u32,
    _numsamplers: u32,
    _ppsamplers: *mut Option<ID3D11SamplerState>,
  ) {
    self.state.unexpected("Context.DSGetSamplers");
  }
  fn DSGetConstantBuffers(
    &self,
    _startslot: u32,
    _numbuffers: u32,
    _ppconstantbuffers: *mut Option<ID3D11Buffer>,
  ) {
    self.state.unexpected("Context.DSGetConstantBuffers");
  }
  fn CSGetShaderResources(
    &self,
    _startslot: u32,
    _numviews: u32,
    _ppshaderresourceviews: *mut Option<ID3D11ShaderResourceView>,
  ) {
    self.state.unexpected("Context.CSGetShaderResources");
  }
  fn CSGetUnorderedAccessViews(
    &self,
    _startslot: u32,
    _numuavs: u32,
    _ppunorderedaccessviews: *mut Option<ID3D11UnorderedAccessView>,
  ) {
    self.state.unexpected("Context.CSGetUnorderedAccessViews");
  }
  fn CSGetShader(
    &self,
    _ppcomputeshader: windows_core::OutRef<ID3D11ComputeShader>,
    _ppclassinstances: windows_core::OutRef<ID3D11ClassInstance>,
    _pnumclassinstances: *mut u32,
  ) {
    self.state.unexpected("Context.CSGetShader");
  }
  fn CSGetSamplers(
    &self,
    _startslot: u32,
    _numsamplers: u32,
    _ppsamplers: *mut Option<ID3D11SamplerState>,
  ) {
    self.state.unexpected("Context.CSGetSamplers");
  }
  fn CSGetConstantBuffers(
    &self,
    _startslot: u32,
    _numbuffers: u32,
    _ppconstantbuffers: *mut Option<ID3D11Buffer>,
  ) {
    self.state.unexpected("Context.CSGetConstantBuffers");
  }
  fn ClearState(&self) {
    self.state.unexpected("Context.ClearState");
  }
  fn Flush(&self) {
    self.state.calls.lock().expect("calls").push("Flush");
  }
  fn GetType(&self) -> D3D11_DEVICE_CONTEXT_TYPE {
    self.state.unexpected("Context.GetType");
    Default::default()
  }
  fn GetContextFlags(&self) -> u32 {
    self.state.unexpected("Context.GetContextFlags");
    Default::default()
  }
  fn FinishCommandList(
    &self,
    _restoredeferredcontextstate: windows_core::BOOL,
    _ppcommandlist: windows_core::OutRef<ID3D11CommandList>,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Context.FinishCommandList");
    Err(E_NOTIMPL.into())
  }
}
impl ID3D11DeviceContext1_Impl for Context_Impl {
  fn CopySubresourceRegion1(
    &self,
    _pdstresource: windows_core::Ref<ID3D11Resource>,
    _dstsubresource: u32,
    _dstx: u32,
    _dsty: u32,
    _dstz: u32,
    _psrcresource: windows_core::Ref<ID3D11Resource>,
    _srcsubresource: u32,
    _psrcbox: *const D3D11_BOX,
    _copyflags: u32,
  ) {
    self.state.unexpected("Context.CopySubresourceRegion1");
  }
  fn UpdateSubresource1(
    &self,
    _pdstresource: windows_core::Ref<ID3D11Resource>,
    _dstsubresource: u32,
    _pdstbox: *const D3D11_BOX,
    _psrcdata: *const core::ffi::c_void,
    _srcrowpitch: u32,
    _srcdepthpitch: u32,
    _copyflags: u32,
  ) {
    self.state.unexpected("Context.UpdateSubresource1");
  }
  fn DiscardResource(&self, _presource: windows_core::Ref<ID3D11Resource>) {
    self.state.unexpected("Context.DiscardResource");
  }
  fn DiscardView(&self, _presourceview: windows_core::Ref<ID3D11View>) {
    self.state.unexpected("Context.DiscardView");
  }
  fn VSSetConstantBuffers1(
    &self,
    _startslot: u32,
    _numbuffers: u32,
    _ppconstantbuffers: *const Option<ID3D11Buffer>,
    _pfirstconstant: *const u32,
    _pnumconstants: *const u32,
  ) {
    self.state.unexpected("Context.VSSetConstantBuffers1");
  }
  fn HSSetConstantBuffers1(
    &self,
    _startslot: u32,
    _numbuffers: u32,
    _ppconstantbuffers: *const Option<ID3D11Buffer>,
    _pfirstconstant: *const u32,
    _pnumconstants: *const u32,
  ) {
    self.state.unexpected("Context.HSSetConstantBuffers1");
  }
  fn DSSetConstantBuffers1(
    &self,
    _startslot: u32,
    _numbuffers: u32,
    _ppconstantbuffers: *const Option<ID3D11Buffer>,
    _pfirstconstant: *const u32,
    _pnumconstants: *const u32,
  ) {
    self.state.unexpected("Context.DSSetConstantBuffers1");
  }
  fn GSSetConstantBuffers1(
    &self,
    _startslot: u32,
    _numbuffers: u32,
    _ppconstantbuffers: *const Option<ID3D11Buffer>,
    _pfirstconstant: *const u32,
    _pnumconstants: *const u32,
  ) {
    self.state.unexpected("Context.GSSetConstantBuffers1");
  }
  fn PSSetConstantBuffers1(
    &self,
    _startslot: u32,
    _numbuffers: u32,
    _ppconstantbuffers: *const Option<ID3D11Buffer>,
    _pfirstconstant: *const u32,
    _pnumconstants: *const u32,
  ) {
    self.state.unexpected("Context.PSSetConstantBuffers1");
  }
  fn CSSetConstantBuffers1(
    &self,
    _startslot: u32,
    _numbuffers: u32,
    _ppconstantbuffers: *const Option<ID3D11Buffer>,
    _pfirstconstant: *const u32,
    _pnumconstants: *const u32,
  ) {
    self.state.unexpected("Context.CSSetConstantBuffers1");
  }
  fn VSGetConstantBuffers1(
    &self,
    _startslot: u32,
    _numbuffers: u32,
    _ppconstantbuffers: windows_core::OutRef<ID3D11Buffer>,
    _pfirstconstant: *mut u32,
    _pnumconstants: *mut u32,
  ) {
    self.state.unexpected("Context.VSGetConstantBuffers1");
  }
  fn HSGetConstantBuffers1(
    &self,
    _startslot: u32,
    _numbuffers: u32,
    _ppconstantbuffers: windows_core::OutRef<ID3D11Buffer>,
    _pfirstconstant: *mut u32,
    _pnumconstants: *mut u32,
  ) {
    self.state.unexpected("Context.HSGetConstantBuffers1");
  }
  fn DSGetConstantBuffers1(
    &self,
    _startslot: u32,
    _numbuffers: u32,
    _ppconstantbuffers: windows_core::OutRef<ID3D11Buffer>,
    _pfirstconstant: *mut u32,
    _pnumconstants: *mut u32,
  ) {
    self.state.unexpected("Context.DSGetConstantBuffers1");
  }
  fn GSGetConstantBuffers1(
    &self,
    _startslot: u32,
    _numbuffers: u32,
    _ppconstantbuffers: windows_core::OutRef<ID3D11Buffer>,
    _pfirstconstant: *mut u32,
    _pnumconstants: *mut u32,
  ) {
    self.state.unexpected("Context.GSGetConstantBuffers1");
  }
  fn PSGetConstantBuffers1(
    &self,
    _startslot: u32,
    _numbuffers: u32,
    _ppconstantbuffers: windows_core::OutRef<ID3D11Buffer>,
    _pfirstconstant: *mut u32,
    _pnumconstants: *mut u32,
  ) {
    self.state.unexpected("Context.PSGetConstantBuffers1");
  }
  fn CSGetConstantBuffers1(
    &self,
    _startslot: u32,
    _numbuffers: u32,
    _ppconstantbuffers: windows_core::OutRef<ID3D11Buffer>,
    _pfirstconstant: *mut u32,
    _pnumconstants: *mut u32,
  ) {
    self.state.unexpected("Context.CSGetConstantBuffers1");
  }
  fn SwapDeviceContextState(
    &self,
    _pstate: windows_core::Ref<ID3DDeviceContextState>,
    _pppreviousstate: windows_core::OutRef<ID3DDeviceContextState>,
  ) {
    self.state.unexpected("Context.SwapDeviceContextState");
  }
  fn ClearView(
    &self,
    _pview: windows_core::Ref<ID3D11View>,
    _color: *const f32,
    _prect: *const windows::Win32::Foundation::RECT,
    _numrects: u32,
  ) {
    self.state.unexpected("Context.ClearView");
  }
  fn DiscardView1(
    &self,
    _presourceview: windows_core::Ref<ID3D11View>,
    _prects: *const windows::Win32::Foundation::RECT,
    _numrects: u32,
  ) {
    self.state.unexpected("Context.DiscardView1");
  }
}
impl ID3D11VideoContext_Impl for Context_Impl {
  fn GetDecoderBuffer(
    &self,
    _pdecoder: windows_core::Ref<ID3D11VideoDecoder>,
    _buffer_type: D3D11_VIDEO_DECODER_BUFFER_TYPE,
    _pbuffersize: *mut u32,
    _ppbuffer: *mut *mut core::ffi::c_void,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Context.GetDecoderBuffer");
    Err(E_NOTIMPL.into())
  }
  fn ReleaseDecoderBuffer(
    &self,
    _pdecoder: windows_core::Ref<ID3D11VideoDecoder>,
    _buffer_type: D3D11_VIDEO_DECODER_BUFFER_TYPE,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Context.ReleaseDecoderBuffer");
    Err(E_NOTIMPL.into())
  }
  fn DecoderBeginFrame(
    &self,
    _pdecoder: windows_core::Ref<ID3D11VideoDecoder>,
    _pview: windows_core::Ref<ID3D11VideoDecoderOutputView>,
    _contentkeysize: u32,
    _pcontentkey: *const core::ffi::c_void,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Context.DecoderBeginFrame");
    Err(E_NOTIMPL.into())
  }
  fn DecoderEndFrame(
    &self,
    _pdecoder: windows_core::Ref<ID3D11VideoDecoder>,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Context.DecoderEndFrame");
    Err(E_NOTIMPL.into())
  }
  fn SubmitDecoderBuffers(
    &self,
    _pdecoder: windows_core::Ref<ID3D11VideoDecoder>,
    _numbuffers: u32,
    _pbufferdesc: *const D3D11_VIDEO_DECODER_BUFFER_DESC,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Context.SubmitDecoderBuffers");
    Err(E_NOTIMPL.into())
  }
  fn DecoderExtension(
    &self,
    _pdecoder: windows_core::Ref<ID3D11VideoDecoder>,
    _pextensiondata: *const D3D11_VIDEO_DECODER_EXTENSION,
  ) -> i32 {
    self.state.unexpected("Context.DecoderExtension");
    Default::default()
  }
  fn VideoProcessorSetOutputTargetRect(
    &self,
    _pvideoprocessor: windows_core::Ref<ID3D11VideoProcessor>,
    _enable: windows_core::BOOL,
    _prect: *const windows::Win32::Foundation::RECT,
  ) {
    let rect = unsafe { *_prect };
    self.state.rects.lock().expect("rects").push((
      "VideoProcessorSetOutputTargetRect",
      [rect.left, rect.top, rect.right, rect.bottom],
    ));
  }
  fn VideoProcessorSetOutputBackgroundColor(
    &self,
    _pvideoprocessor: windows_core::Ref<ID3D11VideoProcessor>,
    _ycbcr: windows_core::BOOL,
    _pcolor: *const D3D11_VIDEO_COLOR,
  ) {
    self
      .state
      .unexpected("Context.VideoProcessorSetOutputBackgroundColor");
  }
  fn VideoProcessorSetOutputColorSpace(
    &self,
    _pvideoprocessor: windows_core::Ref<ID3D11VideoProcessor>,
    _pcolorspace: *const D3D11_VIDEO_PROCESSOR_COLOR_SPACE,
  ) {
    self
      .state
      .unexpected("Context.VideoProcessorSetOutputColorSpace");
  }
  fn VideoProcessorSetOutputAlphaFillMode(
    &self,
    _pvideoprocessor: windows_core::Ref<ID3D11VideoProcessor>,
    _alphafillmode: D3D11_VIDEO_PROCESSOR_ALPHA_FILL_MODE,
    _streamindex: u32,
  ) {
    self
      .state
      .unexpected("Context.VideoProcessorSetOutputAlphaFillMode");
  }
  fn VideoProcessorSetOutputConstriction(
    &self,
    _pvideoprocessor: windows_core::Ref<ID3D11VideoProcessor>,
    _enable: windows_core::BOOL,
    _size: &windows::Win32::Foundation::SIZE,
  ) {
    self
      .state
      .unexpected("Context.VideoProcessorSetOutputConstriction");
  }
  fn VideoProcessorSetOutputStereoMode(
    &self,
    _pvideoprocessor: windows_core::Ref<ID3D11VideoProcessor>,
    _enable: windows_core::BOOL,
  ) {
    self
      .state
      .unexpected("Context.VideoProcessorSetOutputStereoMode");
  }
  fn VideoProcessorSetOutputExtension(
    &self,
    _pvideoprocessor: windows_core::Ref<ID3D11VideoProcessor>,
    _pextensionguid: *const windows_core::GUID,
    _datasize: u32,
    _pdata: *const core::ffi::c_void,
  ) -> i32 {
    self
      .state
      .unexpected("Context.VideoProcessorSetOutputExtension");
    Default::default()
  }
  fn VideoProcessorGetOutputTargetRect(
    &self,
    _pvideoprocessor: windows_core::Ref<ID3D11VideoProcessor>,
    _enabled: *mut windows_core::BOOL,
    _prect: *mut windows::Win32::Foundation::RECT,
  ) {
    self
      .state
      .unexpected("Context.VideoProcessorGetOutputTargetRect");
  }
  fn VideoProcessorGetOutputBackgroundColor(
    &self,
    _pvideoprocessor: windows_core::Ref<ID3D11VideoProcessor>,
    _pycbcr: *mut windows_core::BOOL,
    _pcolor: *mut D3D11_VIDEO_COLOR,
  ) {
    self
      .state
      .unexpected("Context.VideoProcessorGetOutputBackgroundColor");
  }
  fn VideoProcessorGetOutputColorSpace(
    &self,
    _pvideoprocessor: windows_core::Ref<ID3D11VideoProcessor>,
    _pcolorspace: *mut D3D11_VIDEO_PROCESSOR_COLOR_SPACE,
  ) {
    self
      .state
      .unexpected("Context.VideoProcessorGetOutputColorSpace");
  }
  fn VideoProcessorGetOutputAlphaFillMode(
    &self,
    _pvideoprocessor: windows_core::Ref<ID3D11VideoProcessor>,
    _palphafillmode: *mut D3D11_VIDEO_PROCESSOR_ALPHA_FILL_MODE,
    _pstreamindex: *mut u32,
  ) {
    self
      .state
      .unexpected("Context.VideoProcessorGetOutputAlphaFillMode");
  }
  fn VideoProcessorGetOutputConstriction(
    &self,
    _pvideoprocessor: windows_core::Ref<ID3D11VideoProcessor>,
    _penabled: *mut windows_core::BOOL,
    _psize: *mut windows::Win32::Foundation::SIZE,
  ) {
    self
      .state
      .unexpected("Context.VideoProcessorGetOutputConstriction");
  }
  fn VideoProcessorGetOutputStereoMode(
    &self,
    _pvideoprocessor: windows_core::Ref<ID3D11VideoProcessor>,
    _penabled: *mut windows_core::BOOL,
  ) {
    self
      .state
      .unexpected("Context.VideoProcessorGetOutputStereoMode");
  }
  fn VideoProcessorGetOutputExtension(
    &self,
    _pvideoprocessor: windows_core::Ref<ID3D11VideoProcessor>,
    _pextensionguid: *const windows_core::GUID,
    _datasize: u32,
    _pdata: *mut core::ffi::c_void,
  ) -> i32 {
    self
      .state
      .unexpected("Context.VideoProcessorGetOutputExtension");
    Default::default()
  }
  fn VideoProcessorSetStreamFrameFormat(
    &self,
    _pvideoprocessor: windows_core::Ref<ID3D11VideoProcessor>,
    _streamindex: u32,
    _frameformat: D3D11_VIDEO_FRAME_FORMAT,
  ) {
    if _frameformat != D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE {
      self.state.unexpected("non-progressive");
    }
  }
  fn VideoProcessorSetStreamColorSpace(
    &self,
    _pvideoprocessor: windows_core::Ref<ID3D11VideoProcessor>,
    _streamindex: u32,
    _pcolorspace: *const D3D11_VIDEO_PROCESSOR_COLOR_SPACE,
  ) {
    self
      .state
      .unexpected("Context.VideoProcessorSetStreamColorSpace");
  }
  fn VideoProcessorSetStreamOutputRate(
    &self,
    _pvideoprocessor: windows_core::Ref<ID3D11VideoProcessor>,
    _streamindex: u32,
    _outputrate: D3D11_VIDEO_PROCESSOR_OUTPUT_RATE,
    _repeatframe: windows_core::BOOL,
    _pcustomrate: *const windows::Win32::Graphics::Dxgi::Common::DXGI_RATIONAL,
  ) {
    self
      .state
      .unexpected("Context.VideoProcessorSetStreamOutputRate");
  }
  fn VideoProcessorSetStreamSourceRect(
    &self,
    _pvideoprocessor: windows_core::Ref<ID3D11VideoProcessor>,
    _streamindex: u32,
    _enable: windows_core::BOOL,
    _prect: *const windows::Win32::Foundation::RECT,
  ) {
    let rect = unsafe { *_prect };
    self.state.rects.lock().expect("rects").push((
      "VideoProcessorSetStreamSourceRect",
      [rect.left, rect.top, rect.right, rect.bottom],
    ));
  }
  fn VideoProcessorSetStreamDestRect(
    &self,
    _pvideoprocessor: windows_core::Ref<ID3D11VideoProcessor>,
    _streamindex: u32,
    _enable: windows_core::BOOL,
    _prect: *const windows::Win32::Foundation::RECT,
  ) {
    let rect = unsafe { *_prect };
    self.state.rects.lock().expect("rects").push((
      "VideoProcessorSetStreamDestRect",
      [rect.left, rect.top, rect.right, rect.bottom],
    ));
  }
  fn VideoProcessorSetStreamAlpha(
    &self,
    _pvideoprocessor: windows_core::Ref<ID3D11VideoProcessor>,
    _streamindex: u32,
    _enable: windows_core::BOOL,
    _alpha: f32,
  ) {
    self
      .state
      .unexpected("Context.VideoProcessorSetStreamAlpha");
  }
  fn VideoProcessorSetStreamPalette(
    &self,
    _pvideoprocessor: windows_core::Ref<ID3D11VideoProcessor>,
    _streamindex: u32,
    _count: u32,
    _pentries: *const u32,
  ) {
    self
      .state
      .unexpected("Context.VideoProcessorSetStreamPalette");
  }
  fn VideoProcessorSetStreamPixelAspectRatio(
    &self,
    _pvideoprocessor: windows_core::Ref<ID3D11VideoProcessor>,
    _streamindex: u32,
    _enable: windows_core::BOOL,
    _psourceaspectratio: *const windows::Win32::Graphics::Dxgi::Common::DXGI_RATIONAL,
    _pdestinationaspectratio: *const windows::Win32::Graphics::Dxgi::Common::DXGI_RATIONAL,
  ) {
    self
      .state
      .unexpected("Context.VideoProcessorSetStreamPixelAspectRatio");
  }
  fn VideoProcessorSetStreamLumaKey(
    &self,
    _pvideoprocessor: windows_core::Ref<ID3D11VideoProcessor>,
    _streamindex: u32,
    _enable: windows_core::BOOL,
    _lower: f32,
    _upper: f32,
  ) {
    self
      .state
      .unexpected("Context.VideoProcessorSetStreamLumaKey");
  }
  fn VideoProcessorSetStreamStereoFormat(
    &self,
    _pvideoprocessor: windows_core::Ref<ID3D11VideoProcessor>,
    _streamindex: u32,
    _enable: windows_core::BOOL,
    _format: D3D11_VIDEO_PROCESSOR_STEREO_FORMAT,
    _leftviewframe0: windows_core::BOOL,
    _baseviewframe0: windows_core::BOOL,
    _flipmode: D3D11_VIDEO_PROCESSOR_STEREO_FLIP_MODE,
    _monooffset: i32,
  ) {
    self
      .state
      .unexpected("Context.VideoProcessorSetStreamStereoFormat");
  }
  fn VideoProcessorSetStreamAutoProcessingMode(
    &self,
    _pvideoprocessor: windows_core::Ref<ID3D11VideoProcessor>,
    _streamindex: u32,
    _enable: windows_core::BOOL,
  ) {
    self
      .state
      .unexpected("Context.VideoProcessorSetStreamAutoProcessingMode");
  }
  fn VideoProcessorSetStreamFilter(
    &self,
    _pvideoprocessor: windows_core::Ref<ID3D11VideoProcessor>,
    _streamindex: u32,
    _filter: D3D11_VIDEO_PROCESSOR_FILTER,
    _enable: windows_core::BOOL,
    _level: i32,
  ) {
    self
      .state
      .unexpected("Context.VideoProcessorSetStreamFilter");
  }
  fn VideoProcessorSetStreamExtension(
    &self,
    _pvideoprocessor: windows_core::Ref<ID3D11VideoProcessor>,
    _streamindex: u32,
    _pextensionguid: *const windows_core::GUID,
    _datasize: u32,
    _pdata: *const core::ffi::c_void,
  ) -> i32 {
    self
      .state
      .unexpected("Context.VideoProcessorSetStreamExtension");
    Default::default()
  }
  fn VideoProcessorGetStreamFrameFormat(
    &self,
    _pvideoprocessor: windows_core::Ref<ID3D11VideoProcessor>,
    _streamindex: u32,
    _pframeformat: *mut D3D11_VIDEO_FRAME_FORMAT,
  ) {
    self
      .state
      .unexpected("Context.VideoProcessorGetStreamFrameFormat");
  }
  fn VideoProcessorGetStreamColorSpace(
    &self,
    _pvideoprocessor: windows_core::Ref<ID3D11VideoProcessor>,
    _streamindex: u32,
    _pcolorspace: *mut D3D11_VIDEO_PROCESSOR_COLOR_SPACE,
  ) {
    self
      .state
      .unexpected("Context.VideoProcessorGetStreamColorSpace");
  }
  fn VideoProcessorGetStreamOutputRate(
    &self,
    _pvideoprocessor: windows_core::Ref<ID3D11VideoProcessor>,
    _streamindex: u32,
    _poutputrate: *mut D3D11_VIDEO_PROCESSOR_OUTPUT_RATE,
    _prepeatframe: *mut windows_core::BOOL,
    _pcustomrate: *mut windows::Win32::Graphics::Dxgi::Common::DXGI_RATIONAL,
  ) {
    self
      .state
      .unexpected("Context.VideoProcessorGetStreamOutputRate");
  }
  fn VideoProcessorGetStreamSourceRect(
    &self,
    _pvideoprocessor: windows_core::Ref<ID3D11VideoProcessor>,
    _streamindex: u32,
    _penabled: *mut windows_core::BOOL,
    _prect: *mut windows::Win32::Foundation::RECT,
  ) {
    self
      .state
      .unexpected("Context.VideoProcessorGetStreamSourceRect");
  }
  fn VideoProcessorGetStreamDestRect(
    &self,
    _pvideoprocessor: windows_core::Ref<ID3D11VideoProcessor>,
    _streamindex: u32,
    _penabled: *mut windows_core::BOOL,
    _prect: *mut windows::Win32::Foundation::RECT,
  ) {
    self
      .state
      .unexpected("Context.VideoProcessorGetStreamDestRect");
  }
  fn VideoProcessorGetStreamAlpha(
    &self,
    _pvideoprocessor: windows_core::Ref<ID3D11VideoProcessor>,
    _streamindex: u32,
    _penabled: *mut windows_core::BOOL,
    _palpha: *mut f32,
  ) {
    self
      .state
      .unexpected("Context.VideoProcessorGetStreamAlpha");
  }
  fn VideoProcessorGetStreamPalette(
    &self,
    _pvideoprocessor: windows_core::Ref<ID3D11VideoProcessor>,
    _streamindex: u32,
    _count: u32,
    _pentries: *mut u32,
  ) {
    self
      .state
      .unexpected("Context.VideoProcessorGetStreamPalette");
  }
  fn VideoProcessorGetStreamPixelAspectRatio(
    &self,
    _pvideoprocessor: windows_core::Ref<ID3D11VideoProcessor>,
    _streamindex: u32,
    _penabled: *mut windows_core::BOOL,
    _psourceaspectratio: *mut windows::Win32::Graphics::Dxgi::Common::DXGI_RATIONAL,
    _pdestinationaspectratio: *mut windows::Win32::Graphics::Dxgi::Common::DXGI_RATIONAL,
  ) {
    self
      .state
      .unexpected("Context.VideoProcessorGetStreamPixelAspectRatio");
  }
  fn VideoProcessorGetStreamLumaKey(
    &self,
    _pvideoprocessor: windows_core::Ref<ID3D11VideoProcessor>,
    _streamindex: u32,
    _penabled: *mut windows_core::BOOL,
    _plower: *mut f32,
    _pupper: *mut f32,
  ) {
    self
      .state
      .unexpected("Context.VideoProcessorGetStreamLumaKey");
  }
  fn VideoProcessorGetStreamStereoFormat(
    &self,
    _pvideoprocessor: windows_core::Ref<ID3D11VideoProcessor>,
    _streamindex: u32,
    _penable: *mut windows_core::BOOL,
    _pformat: *mut D3D11_VIDEO_PROCESSOR_STEREO_FORMAT,
    _pleftviewframe0: *mut windows_core::BOOL,
    _pbaseviewframe0: *mut windows_core::BOOL,
    _pflipmode: *mut D3D11_VIDEO_PROCESSOR_STEREO_FLIP_MODE,
    _monooffset: *mut i32,
  ) {
    self
      .state
      .unexpected("Context.VideoProcessorGetStreamStereoFormat");
  }
  fn VideoProcessorGetStreamAutoProcessingMode(
    &self,
    _pvideoprocessor: windows_core::Ref<ID3D11VideoProcessor>,
    _streamindex: u32,
    _penabled: *mut windows_core::BOOL,
  ) {
    self
      .state
      .unexpected("Context.VideoProcessorGetStreamAutoProcessingMode");
  }
  fn VideoProcessorGetStreamFilter(
    &self,
    _pvideoprocessor: windows_core::Ref<ID3D11VideoProcessor>,
    _streamindex: u32,
    _filter: D3D11_VIDEO_PROCESSOR_FILTER,
    _penabled: *mut windows_core::BOOL,
    _plevel: *mut i32,
  ) {
    self
      .state
      .unexpected("Context.VideoProcessorGetStreamFilter");
  }
  fn VideoProcessorGetStreamExtension(
    &self,
    _pvideoprocessor: windows_core::Ref<ID3D11VideoProcessor>,
    _streamindex: u32,
    _pextensionguid: *const windows_core::GUID,
    _datasize: u32,
    _pdata: *mut core::ffi::c_void,
  ) -> i32 {
    self
      .state
      .unexpected("Context.VideoProcessorGetStreamExtension");
    Default::default()
  }
  fn VideoProcessorBlt(
    &self,
    _pvideoprocessor: windows_core::Ref<ID3D11VideoProcessor>,
    _pview: windows_core::Ref<ID3D11VideoProcessorOutputView>,
    _outputframe: u32,
    _streamcount: u32,
    _pstreams: *const D3D11_VIDEO_PROCESSOR_STREAM,
  ) -> windows_core::Result<()> {
    self.state.call("VideoProcessorBlt")?;
    if _streamcount != 1 || _pstreams.is_null() || !unsafe { (*_pstreams).Enable.as_bool() } {
      return Err(E_FAIL.into());
    }
    Ok(())
  }
  fn NegotiateCryptoSessionKeyExchange(
    &self,
    _pcryptosession: windows_core::Ref<ID3D11CryptoSession>,
    _datasize: u32,
    _pdata: *mut core::ffi::c_void,
  ) -> windows_core::Result<()> {
    self
      .state
      .unexpected("Context.NegotiateCryptoSessionKeyExchange");
    Err(E_NOTIMPL.into())
  }
  fn EncryptionBlt(
    &self,
    _pcryptosession: windows_core::Ref<ID3D11CryptoSession>,
    _psrcsurface: windows_core::Ref<ID3D11Texture2D>,
    _pdstsurface: windows_core::Ref<ID3D11Texture2D>,
    _ivsize: u32,
    _piv: *mut core::ffi::c_void,
  ) {
    self.state.unexpected("Context.EncryptionBlt");
  }
  fn DecryptionBlt(
    &self,
    _pcryptosession: windows_core::Ref<ID3D11CryptoSession>,
    _psrcsurface: windows_core::Ref<ID3D11Texture2D>,
    _pdstsurface: windows_core::Ref<ID3D11Texture2D>,
    _pencryptedblockinfo: *const D3D11_ENCRYPTED_BLOCK_INFO,
    _contentkeysize: u32,
    _pcontentkey: *const core::ffi::c_void,
    _ivsize: u32,
    _piv: *mut core::ffi::c_void,
  ) {
    self.state.unexpected("Context.DecryptionBlt");
  }
  fn StartSessionKeyRefresh(
    &self,
    _pcryptosession: windows_core::Ref<ID3D11CryptoSession>,
    _randomnumbersize: u32,
    _prandomnumber: *mut core::ffi::c_void,
  ) {
    self.state.unexpected("Context.StartSessionKeyRefresh");
  }
  fn FinishSessionKeyRefresh(&self, _pcryptosession: windows_core::Ref<ID3D11CryptoSession>) {
    self.state.unexpected("Context.FinishSessionKeyRefresh");
  }
  fn GetEncryptionBltKey(
    &self,
    _pcryptosession: windows_core::Ref<ID3D11CryptoSession>,
    _keysize: u32,
    _preadbackkey: *mut core::ffi::c_void,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Context.GetEncryptionBltKey");
    Err(E_NOTIMPL.into())
  }
  fn NegotiateAuthenticatedChannelKeyExchange(
    &self,
    _pchannel: windows_core::Ref<ID3D11AuthenticatedChannel>,
    _datasize: u32,
    _pdata: *mut core::ffi::c_void,
  ) -> windows_core::Result<()> {
    self
      .state
      .unexpected("Context.NegotiateAuthenticatedChannelKeyExchange");
    Err(E_NOTIMPL.into())
  }
  fn QueryAuthenticatedChannel(
    &self,
    _pchannel: windows_core::Ref<ID3D11AuthenticatedChannel>,
    _inputsize: u32,
    _pinput: *const core::ffi::c_void,
    _outputsize: u32,
    _poutput: *mut core::ffi::c_void,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Context.QueryAuthenticatedChannel");
    Err(E_NOTIMPL.into())
  }
  fn ConfigureAuthenticatedChannel(
    &self,
    _pchannel: windows_core::Ref<ID3D11AuthenticatedChannel>,
    _inputsize: u32,
    _pinput: *const core::ffi::c_void,
    _poutput: *mut D3D11_AUTHENTICATED_CONFIGURE_OUTPUT,
  ) -> windows_core::Result<()> {
    self
      .state
      .unexpected("Context.ConfigureAuthenticatedChannel");
    Err(E_NOTIMPL.into())
  }
  fn VideoProcessorSetStreamRotation(
    &self,
    _pvideoprocessor: windows_core::Ref<ID3D11VideoProcessor>,
    _streamindex: u32,
    _enable: windows_core::BOOL,
    _rotation: D3D11_VIDEO_PROCESSOR_ROTATION,
  ) {
    self
      .state
      .unexpected("Context.VideoProcessorSetStreamRotation");
  }
  fn VideoProcessorGetStreamRotation(
    &self,
    _pvideoprocessor: windows_core::Ref<ID3D11VideoProcessor>,
    _streamindex: u32,
    _penable: *mut windows_core::BOOL,
    _protation: *mut D3D11_VIDEO_PROCESSOR_ROTATION,
  ) {
    self
      .state
      .unexpected("Context.VideoProcessorGetStreamRotation");
  }
}
impl ID3D11VideoContext1_Impl for Context_Impl {
  fn SubmitDecoderBuffers1(
    &self,
    _pdecoder: windows_core::Ref<ID3D11VideoDecoder>,
    _numbuffers: u32,
    _pbufferdesc: *const D3D11_VIDEO_DECODER_BUFFER_DESC1,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Context.SubmitDecoderBuffers1");
    Err(E_NOTIMPL.into())
  }
  fn GetDataForNewHardwareKey(
    &self,
    _pcryptosession: windows_core::Ref<ID3D11CryptoSession>,
    _privateinputsize: u32,
    _pprivatinputdata: *const core::ffi::c_void,
  ) -> windows_core::Result<u64> {
    self.state.unexpected("Context.GetDataForNewHardwareKey");
    Err(E_NOTIMPL.into())
  }
  fn CheckCryptoSessionStatus(
    &self,
    _pcryptosession: windows_core::Ref<ID3D11CryptoSession>,
  ) -> windows_core::Result<D3D11_CRYPTO_SESSION_STATUS> {
    self.state.unexpected("Context.CheckCryptoSessionStatus");
    Err(E_NOTIMPL.into())
  }
  fn DecoderEnableDownsampling(
    &self,
    _pdecoder: windows_core::Ref<ID3D11VideoDecoder>,
    _inputcolorspace: windows::Win32::Graphics::Dxgi::Common::DXGI_COLOR_SPACE_TYPE,
    _poutputdesc: *const D3D11_VIDEO_SAMPLE_DESC,
    _referenceframecount: u32,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Context.DecoderEnableDownsampling");
    Err(E_NOTIMPL.into())
  }
  fn DecoderUpdateDownsampling(
    &self,
    _pdecoder: windows_core::Ref<ID3D11VideoDecoder>,
    _poutputdesc: *const D3D11_VIDEO_SAMPLE_DESC,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Context.DecoderUpdateDownsampling");
    Err(E_NOTIMPL.into())
  }
  fn VideoProcessorSetOutputColorSpace1(
    &self,
    _pvideoprocessor: windows_core::Ref<ID3D11VideoProcessor>,
    _colorspace: windows::Win32::Graphics::Dxgi::Common::DXGI_COLOR_SPACE_TYPE,
  ) {
    if _colorspace != DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709 {
      self.state.unexpected("non-709-output");
    }
    self.state.calls.lock().expect("calls").push("SetOutput709");
  }
  fn VideoProcessorSetOutputShaderUsage(
    &self,
    _pvideoprocessor: windows_core::Ref<ID3D11VideoProcessor>,
    _shaderusage: windows_core::BOOL,
  ) {
    self
      .state
      .unexpected("Context.VideoProcessorSetOutputShaderUsage");
  }
  fn VideoProcessorGetOutputColorSpace1(
    &self,
    _pvideoprocessor: windows_core::Ref<ID3D11VideoProcessor>,
    _pcolorspace: *mut windows::Win32::Graphics::Dxgi::Common::DXGI_COLOR_SPACE_TYPE,
  ) {
    self
      .state
      .unexpected("Context.VideoProcessorGetOutputColorSpace1");
  }
  fn VideoProcessorGetOutputShaderUsage(
    &self,
    _pvideoprocessor: windows_core::Ref<ID3D11VideoProcessor>,
    _pshaderusage: *mut windows_core::BOOL,
  ) {
    self
      .state
      .unexpected("Context.VideoProcessorGetOutputShaderUsage");
  }
  fn VideoProcessorSetStreamColorSpace1(
    &self,
    _pvideoprocessor: windows_core::Ref<ID3D11VideoProcessor>,
    _streamindex: u32,
    _colorspace: windows::Win32::Graphics::Dxgi::Common::DXGI_COLOR_SPACE_TYPE,
  ) {
    if _colorspace != DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709 {
      self.state.unexpected("non-709-input");
    }
    self.state.calls.lock().expect("calls").push("SetInput709");
  }
  fn VideoProcessorSetStreamMirror(
    &self,
    _pvideoprocessor: windows_core::Ref<ID3D11VideoProcessor>,
    _streamindex: u32,
    _enable: windows_core::BOOL,
    _fliphorizontal: windows_core::BOOL,
    _flipvertical: windows_core::BOOL,
  ) {
    self
      .state
      .unexpected("Context.VideoProcessorSetStreamMirror");
  }
  fn VideoProcessorGetStreamColorSpace1(
    &self,
    _pvideoprocessor: windows_core::Ref<ID3D11VideoProcessor>,
    _streamindex: u32,
    _pcolorspace: *mut windows::Win32::Graphics::Dxgi::Common::DXGI_COLOR_SPACE_TYPE,
  ) {
    self
      .state
      .unexpected("Context.VideoProcessorGetStreamColorSpace1");
  }
  fn VideoProcessorGetStreamMirror(
    &self,
    _pvideoprocessor: windows_core::Ref<ID3D11VideoProcessor>,
    _streamindex: u32,
    _penable: *mut windows_core::BOOL,
    _pfliphorizontal: *mut windows_core::BOOL,
    _pflipvertical: *mut windows_core::BOOL,
  ) {
    self
      .state
      .unexpected("Context.VideoProcessorGetStreamMirror");
  }
  fn VideoProcessorGetBehaviorHints(
    &self,
    _pvideoprocessor: windows_core::Ref<ID3D11VideoProcessor>,
    _outputwidth: u32,
    _outputheight: u32,
    _outputformat: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
    _streamcount: u32,
    _pstreams: *const D3D11_VIDEO_PROCESSOR_STREAM_BEHAVIOR_HINT,
  ) -> windows_core::Result<u32> {
    self
      .state
      .unexpected("Context.VideoProcessorGetBehaviorHints");
    Err(E_NOTIMPL.into())
  }
}
impl ID3D10Multithread_Impl for Context_Impl {
  fn Enter(&self) {
    self.state.unexpected("Context.Enter");
  }
  fn Leave(&self) {
    self.state.unexpected("Context.Leave");
  }
  fn SetMultithreadProtected(&self, _bmtprotect: windows_core::BOOL) -> windows_core::BOOL {
    let old = self.state.protected.load(Ordering::Acquire);
    if !self.state.reject_protection.load(Ordering::Acquire) {
      self
        .state
        .protected
        .store(_bmtprotect.as_bool(), Ordering::Release);
    }
    old.into()
  }
  fn GetMultithreadProtected(&self) -> windows_core::BOOL {
    self.state.protected.load(Ordering::Acquire).into()
  }
}
#[implement(ID3D11Texture2D, IDXGIResource)]
struct Texture {
  pub state: Arc<State>,
  desc: D3D11_TEXTURE2D_DESC,
}
impl ID3D11DeviceChild_Impl for Texture_Impl {
  fn GetDevice(&self, _ppdevice: windows_core::OutRef<ID3D11Device>) {
    self.state.unexpected("Texture.GetDevice");
  }
  fn GetPrivateData(
    &self,
    _guid: *const windows_core::GUID,
    _pdatasize: *mut u32,
    _pdata: *mut core::ffi::c_void,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Texture.GetPrivateData");
    Err(E_NOTIMPL.into())
  }
  fn SetPrivateData(
    &self,
    _guid: *const windows_core::GUID,
    _datasize: u32,
    _pdata: *const core::ffi::c_void,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Texture.SetPrivateData");
    Err(E_NOTIMPL.into())
  }
  fn SetPrivateDataInterface(
    &self,
    _guid: *const windows_core::GUID,
    _pdata: windows_core::Ref<windows_core::IUnknown>,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Texture.SetPrivateDataInterface");
    Err(E_NOTIMPL.into())
  }
}
impl ID3D11Resource_Impl for Texture_Impl {
  fn GetType(&self, _presourcedimension: *mut D3D11_RESOURCE_DIMENSION) {
    self.state.unexpected("Texture.GetType");
  }
  fn SetEvictionPriority(&self, _evictionpriority: u32) {
    self.state.unexpected("Texture.SetEvictionPriority");
  }
  fn GetEvictionPriority(&self) -> u32 {
    self.state.unexpected("Texture.GetEvictionPriority");
    Default::default()
  }
}
impl ID3D11Texture2D_Impl for Texture_Impl {
  fn GetDesc(&self, _pdesc: *mut D3D11_TEXTURE2D_DESC) {
    unsafe {
      *_pdesc = self.desc;
    }
  }
}
#[implement(ID3D11VideoProcessorEnumerator)]
struct Enumerator {
  pub state: Arc<State>,
}
impl ID3D11DeviceChild_Impl for Enumerator_Impl {
  fn GetDevice(&self, _ppdevice: windows_core::OutRef<ID3D11Device>) {
    self.state.unexpected("Enumerator.GetDevice");
  }
  fn GetPrivateData(
    &self,
    _guid: *const windows_core::GUID,
    _pdatasize: *mut u32,
    _pdata: *mut core::ffi::c_void,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Enumerator.GetPrivateData");
    Err(E_NOTIMPL.into())
  }
  fn SetPrivateData(
    &self,
    _guid: *const windows_core::GUID,
    _datasize: u32,
    _pdata: *const core::ffi::c_void,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Enumerator.SetPrivateData");
    Err(E_NOTIMPL.into())
  }
  fn SetPrivateDataInterface(
    &self,
    _guid: *const windows_core::GUID,
    _pdata: windows_core::Ref<windows_core::IUnknown>,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Enumerator.SetPrivateDataInterface");
    Err(E_NOTIMPL.into())
  }
}
impl ID3D11VideoProcessorEnumerator_Impl for Enumerator_Impl {
  fn GetVideoProcessorContentDesc(
    &self,
    _pcontentdesc: *mut D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
  ) -> windows_core::Result<()> {
    self
      .state
      .unexpected("Enumerator.GetVideoProcessorContentDesc");
    Err(E_NOTIMPL.into())
  }
  fn CheckVideoProcessorFormat(
    &self,
    _format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
  ) -> windows_core::Result<u32> {
    self.state.call("CheckVideoProcessorFormat")?;
    Ok(if self.state.formats.load(Ordering::Acquire) {
      (D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT.0 | D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT.0)
        as u32
    } else {
      0
    })
  }
  fn GetVideoProcessorCaps(
    &self,
    _pcaps: *mut D3D11_VIDEO_PROCESSOR_CAPS,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Enumerator.GetVideoProcessorCaps");
    Err(E_NOTIMPL.into())
  }
  fn GetVideoProcessorRateConversionCaps(
    &self,
    _typeindex: u32,
    _pcaps: *mut D3D11_VIDEO_PROCESSOR_RATE_CONVERSION_CAPS,
  ) -> windows_core::Result<()> {
    self
      .state
      .unexpected("Enumerator.GetVideoProcessorRateConversionCaps");
    Err(E_NOTIMPL.into())
  }
  fn GetVideoProcessorCustomRate(
    &self,
    _typeindex: u32,
    _customrateindex: u32,
    _prate: *mut D3D11_VIDEO_PROCESSOR_CUSTOM_RATE,
  ) -> windows_core::Result<()> {
    self
      .state
      .unexpected("Enumerator.GetVideoProcessorCustomRate");
    Err(E_NOTIMPL.into())
  }
  fn GetVideoProcessorFilterRange(
    &self,
    _filter: D3D11_VIDEO_PROCESSOR_FILTER,
  ) -> windows_core::Result<D3D11_VIDEO_PROCESSOR_FILTER_RANGE> {
    self
      .state
      .unexpected("Enumerator.GetVideoProcessorFilterRange");
    Err(E_NOTIMPL.into())
  }
}
#[implement(ID3D11VideoProcessor)]
struct Processor {
  pub state: Arc<State>,
}
impl ID3D11DeviceChild_Impl for Processor_Impl {
  fn GetDevice(&self, _ppdevice: windows_core::OutRef<ID3D11Device>) {
    self.state.unexpected("Processor.GetDevice");
  }
  fn GetPrivateData(
    &self,
    _guid: *const windows_core::GUID,
    _pdatasize: *mut u32,
    _pdata: *mut core::ffi::c_void,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Processor.GetPrivateData");
    Err(E_NOTIMPL.into())
  }
  fn SetPrivateData(
    &self,
    _guid: *const windows_core::GUID,
    _datasize: u32,
    _pdata: *const core::ffi::c_void,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Processor.SetPrivateData");
    Err(E_NOTIMPL.into())
  }
  fn SetPrivateDataInterface(
    &self,
    _guid: *const windows_core::GUID,
    _pdata: windows_core::Ref<windows_core::IUnknown>,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Processor.SetPrivateDataInterface");
    Err(E_NOTIMPL.into())
  }
}
impl ID3D11VideoProcessor_Impl for Processor_Impl {
  fn GetContentDesc(&self, _pdesc: *mut D3D11_VIDEO_PROCESSOR_CONTENT_DESC) {
    self.state.unexpected("Processor.GetContentDesc");
  }
  fn GetRateConversionCaps(&self, _pcaps: *mut D3D11_VIDEO_PROCESSOR_RATE_CONVERSION_CAPS) {
    self.state.unexpected("Processor.GetRateConversionCaps");
  }
}
#[implement(ID3D11VideoProcessorInputView)]
struct InputView {
  pub state: Arc<State>,
  resource: ID3D11Resource,
}
impl ID3D11DeviceChild_Impl for InputView_Impl {
  fn GetDevice(&self, _ppdevice: windows_core::OutRef<ID3D11Device>) {
    self.state.unexpected("InputView.GetDevice");
  }
  fn GetPrivateData(
    &self,
    _guid: *const windows_core::GUID,
    _pdatasize: *mut u32,
    _pdata: *mut core::ffi::c_void,
  ) -> windows_core::Result<()> {
    self.state.unexpected("InputView.GetPrivateData");
    Err(E_NOTIMPL.into())
  }
  fn SetPrivateData(
    &self,
    _guid: *const windows_core::GUID,
    _datasize: u32,
    _pdata: *const core::ffi::c_void,
  ) -> windows_core::Result<()> {
    self.state.unexpected("InputView.SetPrivateData");
    Err(E_NOTIMPL.into())
  }
  fn SetPrivateDataInterface(
    &self,
    _guid: *const windows_core::GUID,
    _pdata: windows_core::Ref<windows_core::IUnknown>,
  ) -> windows_core::Result<()> {
    self.state.unexpected("InputView.SetPrivateDataInterface");
    Err(E_NOTIMPL.into())
  }
}
impl ID3D11View_Impl for InputView_Impl {
  fn GetResource(&self, _ppresource: windows_core::OutRef<ID3D11Resource>) {
    if _ppresource.write(Some(self.resource.clone())).is_err() {
      self.state.unexpected("null-resource-output");
    }
  }
}
impl ID3D11VideoProcessorInputView_Impl for InputView_Impl {
  fn GetDesc(&self, _pdesc: *mut D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC) {
    self.state.unexpected("InputView.GetDesc");
  }
}
#[implement(ID3D11VideoProcessorOutputView)]
struct OutputView {
  pub state: Arc<State>,
  resource: ID3D11Resource,
}
impl ID3D11DeviceChild_Impl for OutputView_Impl {
  fn GetDevice(&self, _ppdevice: windows_core::OutRef<ID3D11Device>) {
    self.state.unexpected("OutputView.GetDevice");
  }
  fn GetPrivateData(
    &self,
    _guid: *const windows_core::GUID,
    _pdatasize: *mut u32,
    _pdata: *mut core::ffi::c_void,
  ) -> windows_core::Result<()> {
    self.state.unexpected("OutputView.GetPrivateData");
    Err(E_NOTIMPL.into())
  }
  fn SetPrivateData(
    &self,
    _guid: *const windows_core::GUID,
    _datasize: u32,
    _pdata: *const core::ffi::c_void,
  ) -> windows_core::Result<()> {
    self.state.unexpected("OutputView.SetPrivateData");
    Err(E_NOTIMPL.into())
  }
  fn SetPrivateDataInterface(
    &self,
    _guid: *const windows_core::GUID,
    _pdata: windows_core::Ref<windows_core::IUnknown>,
  ) -> windows_core::Result<()> {
    self.state.unexpected("OutputView.SetPrivateDataInterface");
    Err(E_NOTIMPL.into())
  }
}
impl ID3D11View_Impl for OutputView_Impl {
  fn GetResource(&self, _ppresource: windows_core::OutRef<ID3D11Resource>) {
    if _ppresource.write(Some(self.resource.clone())).is_err() {
      self.state.unexpected("null-resource-output");
    }
  }
}
impl ID3D11VideoProcessorOutputView_Impl for OutputView_Impl {
  fn GetDesc(&self, _pdesc: *mut D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC) {
    self.state.unexpected("OutputView.GetDesc");
  }
}
#[implement(ID3D11Query)]
struct Query {
  pub state: Arc<State>,
  desc: D3D11_QUERY_DESC,
}
impl ID3D11DeviceChild_Impl for Query_Impl {
  fn GetDevice(&self, _ppdevice: windows_core::OutRef<ID3D11Device>) {
    self.state.unexpected("Query.GetDevice");
  }
  fn GetPrivateData(
    &self,
    _guid: *const windows_core::GUID,
    _pdatasize: *mut u32,
    _pdata: *mut core::ffi::c_void,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Query.GetPrivateData");
    Err(E_NOTIMPL.into())
  }
  fn SetPrivateData(
    &self,
    _guid: *const windows_core::GUID,
    _datasize: u32,
    _pdata: *const core::ffi::c_void,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Query.SetPrivateData");
    Err(E_NOTIMPL.into())
  }
  fn SetPrivateDataInterface(
    &self,
    _guid: *const windows_core::GUID,
    _pdata: windows_core::Ref<windows_core::IUnknown>,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Query.SetPrivateDataInterface");
    Err(E_NOTIMPL.into())
  }
}
impl ID3D11Asynchronous_Impl for Query_Impl {
  fn GetDataSize(&self) -> u32 {
    self.state.unexpected("Query.GetDataSize");
    Default::default()
  }
}
impl ID3D11Query_Impl for Query_Impl {
  fn GetDesc(&self, _pdesc: *mut D3D11_QUERY_DESC) {
    unsafe {
      *_pdesc = self.desc;
    }
  }
}

impl Drop for Texture {
  fn drop(&mut self) {
    self.state.live.fetch_sub(1, Ordering::AcqRel);
  }
}
pub(crate) fn devices() -> (ID3D11Device, ID3D11DeviceContext, Arc<State>) {
  let state = Arc::new(State {
    calls: Mutex::new(Vec::new()),
    unexpected: Mutex::new(Vec::new()),
    protected: AtomicBool::new(false),
    reject_protection: AtomicBool::new(false),
    formats: AtomicBool::new(true),
    creates: AtomicUsize::new(0),
    fail_at: AtomicUsize::new(0),
    live: AtomicUsize::new(0),
    ready: AtomicBool::new(true),
    ticks: AtomicU64::new(100),
    failure: Mutex::new(None),
    dispatches: Mutex::new(Vec::new()),
    constants: Mutex::new(Vec::new()),
    textures: Mutex::new(Vec::new()),
    rects: Mutex::new(Vec::new()),
  });
  let context: ID3D11DeviceContext1 = Context {
    state: state.clone(),
  }
  .into();
  let context = context.cast().expect("base immediate context");
  (
    Device {
      state: state.clone(),
    }
    .into(),
    context,
    state,
  )
}
pub(crate) fn fixture() -> (Gpu, Arc<State>) {
  let (device, context, state) = devices();
  let gpu =
    Gpu::from_device(device, context, 1280, 720, None, 60).expect("mock D3D driver contract");
  (gpu, state)
}
#[test]
fn a_high_frame_rate_pool_doubles_and_converts_into_every_surface() {
  let (device, context, state) = devices();
  let gpu = Gpu::from_device(device, context, 1280, 720, None, 120).expect("120 FPS pool");
  assert_eq!(gpu.surfaces.len(), 16);
  assert_eq!(
    state.live.load(Ordering::Acquire),
    18,
    "two caches and sixteen surfaces"
  );
  assert_eq!(
    gpu.bytes,
    limits::pool_bytes(1280, 720, 1280, 720, ConversionKind::Gpu, 16).expect("budget")
  );
  for index in 0..16 {
    gpu
      .convert(&gpu.desktop, index)
      .expect("each surface has its own view");
  }
  assert!(gpu.convert(&gpu.desktop, 16).is_err());
}
#[test]
fn fixed_gpu_pool_converts_with_reviewed_color_spaces_without_cpu_access_or_per_frame_views() {
  let (gpu, state) = fixture();
  assert_eq!(gpu.surfaces.len(), 8);
  assert_eq!(state.live.load(Ordering::Acquire), 10);
  assert!(state.protected.load(Ordering::Acquire));
  let before = state.calls.lock().expect("calls").len();
  for i in 0..32 {
    gpu
      .convert(
        if i % 2 == 0 {
          &gpu.desktop
        } else {
          &gpu.scratch
        },
        i % 8,
      )
      .expect("GPU conversion");
  }
  let calls = state.calls.lock().expect("calls");
  assert_eq!(calls.len() - before, 32);
  assert!(calls.contains(&"SetInput709"));
  assert!(calls.contains(&"SetOutput709"));
  drop(calls);
  assert!(gpu.convert(&gpu.surfaces[0], 0).is_err());
  assert!(gpu.convert(&gpu.desktop, 8).is_err());
  *state.failure.lock().expect("failure") = Some("VideoProcessorBlt");
  assert!(gpu.convert(&gpu.desktop, 0).is_err());
  assert!(state.unexpected.lock().expect("unexpected").is_empty());
  drop(gpu);
  assert_eq!(state.live.load(Ordering::Acquire), 0);
}
#[test]
fn missing_formats_ignored_protection_and_partial_texture_allocation_are_explicit_and_clean() {
  for index in 1..=10 {
    let (device, context, state) = devices();
    state.fail_at.store(index, Ordering::Release);
    assert!(Gpu::from_device(device, context, 1280, 720, None, 60).is_err());
    assert_eq!(
      state.live.load(Ordering::Acquire),
      0,
      "texture failure {index}"
    );
  }
  let (device, context, state) = devices();
  state.formats.store(false, Ordering::Release);
  assert_eq!(
    Gpu::from_device(device, context, 1280, 720, None, 60)
      .err()
      .map(|e| e.kind),
    Some(ErrorKind::Capability)
  );
  assert_eq!(state.creates.load(Ordering::Acquire), 0);
  let (device, context, state) = devices();
  state.reject_protection.store(true, Ordering::Release);
  assert!(Gpu::from_device(device, context, 1280, 720, None, 60).is_err());
  assert_eq!(state.creates.load(Ordering::Acquire), 0);
}
#[test]
fn delayed_gpu_queries_are_bounded_nonflushing_and_do_not_force_completion() {
  let (gpu, state) = fixture();
  let mut queries = super::super::telemetry::GpuTimers::new(&gpu.device).expect("queries");
  let mut timings = super::super::telemetry::Timings::default();
  state.ready.store(false, Ordering::Release);
  for _ in 0..4 {
    let index = queries.begin(&gpu.context).expect("free query");
    queries.end(&gpu.context, index);
  }
  assert!(queries.begin(&gpu.context).is_none());
  queries
    .collect(&gpu.context, &mut timings)
    .expect("pending");
  assert_eq!(timings.gpu_conversion.summary().count, 0);
  state.ready.store(true, Ordering::Release);
  queries
    .collect(&gpu.context, &mut timings)
    .expect("completed");
  assert_eq!(timings.gpu_conversion.summary().count, 4);
  assert!(queries.begin(&gpu.context).is_some());
  assert!(state.unexpected.lock().expect("unexpected").is_empty());
}
impl IDXGIObject_Impl for Texture_Impl {
  fn SetPrivateData(
    &self,
    _name: *const windows_core::GUID,
    _datasize: u32,
    _pdata: *const core::ffi::c_void,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Texture.SetPrivateData");
    Err(E_NOTIMPL.into())
  }
  fn SetPrivateDataInterface(
    &self,
    _name: *const windows_core::GUID,
    _punknown: windows_core::Ref<windows_core::IUnknown>,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Texture.SetPrivateDataInterface");
    Err(E_NOTIMPL.into())
  }
  fn GetPrivateData(
    &self,
    _name: *const windows_core::GUID,
    _pdatasize: *mut u32,
    _pdata: *mut core::ffi::c_void,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Texture.GetPrivateData");
    Err(E_NOTIMPL.into())
  }
  fn GetParent(
    &self,
    _riid: *const windows_core::GUID,
    _ppparent: *mut *mut core::ffi::c_void,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Texture.GetParent");
    Err(E_NOTIMPL.into())
  }
}
impl IDXGIDeviceSubObject_Impl for Texture_Impl {
  fn GetDevice(
    &self,
    _riid: *const windows_core::GUID,
    _ppdevice: *mut *mut core::ffi::c_void,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Texture.GetDevice");
    Err(E_NOTIMPL.into())
  }
}
impl IDXGIResource_Impl for Texture_Impl {
  fn GetSharedHandle(&self) -> windows_core::Result<windows::Win32::Foundation::HANDLE> {
    self.state.unexpected("Texture.GetSharedHandle");
    Err(E_NOTIMPL.into())
  }
  fn GetUsage(&self) -> windows_core::Result<DXGI_USAGE> {
    self.state.unexpected("Texture.GetUsage");
    Err(E_NOTIMPL.into())
  }
  fn SetEvictionPriority(
    &self,
    _evictionpriority: DXGI_RESOURCE_PRIORITY,
  ) -> windows_core::Result<()> {
    self.state.unexpected("Texture.SetEvictionPriority");
    Err(E_NOTIMPL.into())
  }
  fn GetEvictionPriority(&self) -> windows_core::Result<DXGI_RESOURCE_PRIORITY> {
    self.state.unexpected("Texture.GetEvictionPriority");
    Err(E_NOTIMPL.into())
  }
}
#[test]
fn region_crops_in_the_conversion_blt_with_source_sized_caches_and_output_sized_surfaces() {
  let (device, context, state) = devices();
  let region = Region {
    x: 100,
    y: 50,
    width: 640,
    height: 360,
  };
  let gpu = Gpu::from_device(device, context, 1280, 720, Some(region), 60).expect("cropped pool");
  assert_eq!(
    (gpu.source_width, gpu.source_height, gpu.width, gpu.height),
    (1280, 720, 640, 360)
  );
  assert_eq!(gpu.bytes, 1280 * 720 * 8 + 640 * 360 * 12);
  let textures = state.textures.lock().expect("textures").clone();
  assert_eq!(
    &textures[..2],
    &[(1280, 720, DXGI_FORMAT_B8G8R8A8_UNORM); 2]
  );
  assert_eq!(&textures[2..], &[(640, 360, DXGI_FORMAT_NV12); 8]);
  assert_eq!(
    *state.rects.lock().expect("rects"),
    [
      ("VideoProcessorSetStreamSourceRect", [100, 50, 740, 410]),
      ("VideoProcessorSetStreamDestRect", [0, 0, 640, 360]),
      ("VideoProcessorSetOutputTargetRect", [0, 0, 640, 360]),
    ]
  );
  gpu
    .convert(&gpu.desktop, 0)
    .expect("crop and convert in one pass");
  assert!(state.unexpected.lock().expect("unexpected").is_empty());
  drop(gpu);
  assert_eq!(state.live.load(Ordering::Acquire), 0);
  // Full-display recording sets no rectangle; out-of-display regions never reach the driver.
  let (device, context, state) = devices();
  Gpu::from_device(device, context, 1280, 720, None, 60).expect("full display");
  assert!(state.rects.lock().expect("rects").is_empty());
  for bad in [
    Region {
      x: 1,
      width: 1280,
      ..region
    },
    Region { y: 400, ..region },
    Region {
      width: 641,
      ..region
    },
    Region {
      width: 8,
      height: 8,
      ..region
    },
    Region {
      x: u32::MAX,
      ..region
    },
  ] {
    let (device, context, state) = devices();
    let error = Gpu::from_device(device, context, 1280, 720, Some(bad), 60)
      .err()
      .expect("refused region");
    assert_eq!(
      (error.stage.as_str(), error.kind),
      ("region", ErrorKind::Capability),
      "{bad:?}"
    );
    assert!(state.calls.lock().expect("calls").is_empty(), "{bad:?}");
  }
}
