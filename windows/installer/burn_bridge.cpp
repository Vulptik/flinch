// Thin native adapter to the unmodified WiX 7 bootstrapper API.
// Burn owns transaction/elevation/rollback; Slint owns the main UI thread.
#include <windows.h>
#include <atomic>
#include <mutex>
#include <new>
#include <dutil.h>
#include <dictutil.h>
#include <BootstrapperApplicationBase.h>
#include <BootstrapperApplication.h>

using Notify = void(__cdecl*)(void*, int, int, int);
static std::mutex engineMutex;
static IBootstrapperEngine* engine = nullptr;
class FlinchApplication;
static FlinchApplication* activeApplication = nullptr; // Protected by engineMutex.
static std::atomic<bool> canceled{false};
static std::atomic<bool> busy{false};
static std::atomic<bool> operationStarted{false};
static std::atomic<int> resultCode{0};
static std::atomic<bool> restartRequired{false};
static std::atomic<HWND> parentWindow{nullptr};
static std::atomic<BOOTSTRAPPER_SCOPE> selectedScope{BOOTSTRAPPER_SCOPE_PER_USER};
static std::atomic<bool> scopeLocked{false};
static std::atomic<bool> selectingShortcuts{false};
static std::atomic<bool> wantMenu{false}, wantDesktop{false};
static std::atomic<BOOTSTRAPPER_ACTION> activeAction{BOOTSTRAPPER_ACTION_UNKNOWN};

// A newer installed Flinch blocks every operation of this older bundle except the one that newer
// bundle asks for itself: during its upgrade it runs this bundle with -uninstall
// -burn.related.upgrade to remove the old registration. Refusing that left a stale older entry in
// Apps (1.1.0 -> 1.2.0 failed with 0x80070666).
static HRESULT NewerRelatedBundleStatus(BOOTSTRAPPER_RELATION_TYPE runBy, BOOTSTRAPPER_ACTION action) {
    return runBy == BOOTSTRAPPER_RELATION_UPGRADE && action == BOOTSTRAPPER_ACTION_UNINSTALL
        ? S_OK
        : HRESULT_FROM_WIN32(ERROR_PRODUCT_VERSION);
}

// Flinch installed both per user and per machine has no single maintenance target; the UI tells
// the user to remove one in Apps. MSI found in neither context while detected stays a generic error.
static HRESULT InstalledScopesStatus(bool hasMachine, bool hasUser) {
    if (hasMachine && hasUser) return HRESULT_FROM_WIN32(ERROR_ALREADY_EXISTS);
    return hasMachine || hasUser ? S_OK : HRESULT_FROM_WIN32(ERROR_UNKNOWN_PRODUCT);
}

// Burn hands a -permachine/-peruser switch to the prerequisite BA, not to this one, and any scope
// this BA passes to Plan overrides it (w702). A quiet fresh install with no switch delivered here
// and nothing installed therefore plans the default scope, so Burn applies the command line or the
// authored per-user default. Existing installations and explicit choices keep their scope.
static BOOTSTRAPPER_SCOPE UnattendedPlanScope(bool locked, bool explicitScope, BOOTSTRAPPER_SCOPE selected) {
    return locked || explicitScope ? selected : BOOTSTRAPPER_SCOPE_DEFAULT;
}

// Quiet operations have no Slint window, but Burn needs an owner HWND for Apply and elevation. The
// owner must belong to a thread that keeps processing messages: the Burn callback thread blocks in
// the SDK pipe loop, and an owner created there left a quiet per-machine uninstall waiting at
// "Launching elevated engine process" with no UAC prompt. Start and Stop are bounded.
class SilentOwner {
    HANDLE thread = nullptr;
    HANDLE ready = nullptr;
    DWORD threadId = 0;
    std::atomic<HWND> window{nullptr};
    std::atomic<HRESULT> startStatus{S_OK};
    static DWORD WINAPI Pump(void* value) {
        auto* self = static_cast<SilentOwner*>(value);
        MSG message;
        PeekMessageW(&message, nullptr, WM_USER, WM_USER, PM_NOREMOVE); // Create the queue first.
        const HWND created = CreateWindowExW(WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE, L"STATIC", L"Flinch installer engine", WS_POPUP, 0, 0, 0, 0, nullptr, nullptr, GetModuleHandleW(nullptr), nullptr);
        self->startStatus = created ? S_OK : HRESULT_FROM_WIN32(GetLastError());
        self->window = created;
        SetEvent(self->ready);
        if (!created) return 1;
        BOOL result;
        while ((result = GetMessageW(&message, nullptr, 0, 0)) > 0) {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        DestroyWindow(created);
        return result == 0 ? 0 : 1;
    }
public:
    static constexpr DWORD TimeoutMs = 5000;
    HWND Window() const { return window.load(); }
    HRESULT Start() {
        if (thread) return E_UNEXPECTED;
        ready = CreateEventW(nullptr, TRUE, FALSE, nullptr);
        if (!ready) return HRESULT_FROM_WIN32(GetLastError());
        thread = CreateThread(nullptr, 0, Pump, this, 0, &threadId);
        if (!thread) {
            const HRESULT status = HRESULT_FROM_WIN32(GetLastError());
            CloseHandle(ready); ready = nullptr;
            return status;
        }
        const DWORD wait = WaitForSingleObject(ready, TimeoutMs);
        if (wait != WAIT_OBJECT_0) {
            const HRESULT status = wait == WAIT_TIMEOUT ? HRESULT_FROM_WIN32(WAIT_TIMEOUT) : HRESULT_FROM_WIN32(GetLastError());
            Stop();
            return status;
        }
        if (FAILED(startStatus.load())) {
            const HRESULT status = startStatus.load();
            Stop();
            return status;
        }
        return S_OK;
    }
    HRESULT Stop() {
        HRESULT status = S_OK;
        window = nullptr;
        if (thread) {
            // The thread created its queue before signalling, so the quit cannot be lost.
            if (WaitForSingleObject(ready, 0) == WAIT_OBJECT_0 && !PostThreadMessageW(threadId, WM_QUIT, 0, 0)) {
                const DWORD error = GetLastError();
                if (WaitForSingleObject(thread, 0) != WAIT_OBJECT_0) status = HRESULT_FROM_WIN32(error);
            }
            const DWORD wait = WaitForSingleObject(thread, TimeoutMs);
            if (wait != WAIT_OBJECT_0 && SUCCEEDED(status)) status = wait == WAIT_TIMEOUT ? HRESULT_FROM_WIN32(WAIT_TIMEOUT) : HRESULT_FROM_WIN32(GetLastError());
            if (wait == WAIT_OBJECT_0) { CloseHandle(thread); thread = nullptr; threadId = 0; }
        }
        if (!thread && ready) { CloseHandle(ready); ready = nullptr; }
        return status;
    }
    ~SilentOwner() { Stop(); } // OnDestroy reports failures; this only covers early exits.
};

class FlinchApplication final : public CBootstrapperApplicationBase {
    Notify notify;
    void* context;
    bool installed = false;
    bool upgrade = false;
    HRESULT relatedStatus = S_OK;
    bool unattended = false;
    bool explicitScope = false;
    bool checkEngine = false;
    bool checkPlan = false;
    bool filesInUseShutdownRequested = false;
    BOOTSTRAPPER_SCOPE existingScope = BOOTSTRAPPER_SCOPE_DEFAULT;
    SilentOwner silentOwner;
    BOOTSTRAPPER_ACTION requested = BOOTSTRAPPER_ACTION_INSTALL;
    BOOTSTRAPPER_RELATION_TYPE runBy = BOOTSTRAPPER_RELATION_NONE;
    void Emit(int kind, int value = 0, int detail = 0) { notify(context, kind, value, detail); }
    void Fail(HRESULT status) {
        busy = false;
        resultCode = static_cast<int>(status);
        Emit(6, static_cast<int>(status));
        if (unattended) m_pEngine->Quit(static_cast<DWORD>(status));
    }
public:
    FlinchApplication(Notify callback, void* state) : notify(callback), context(state) {}
    void RequestCancel() {
        canceled = true;
        // Use the SDK's cancellation state too: inherited cache/MSI callbacks
        // check it, and deliberately suppress cancellation during rollback.
        PromptCancel(nullptr, TRUE, nullptr, nullptr);
    }
    void ResetCancellation() {
        EnterCriticalSection(&m_csCanceled);
        m_fCanceled = FALSE;
        LeaveCriticalSection(&m_csCanceled);
        canceled = false;
    }
    STDMETHODIMP OnCreate(IBootstrapperEngine* value, BOOTSTRAPPER_COMMAND* command) override {
        const HRESULT status = CBootstrapperApplicationBase::OnCreate(value, command);
        if (FAILED(status)) return status;
        { std::lock_guard<std::mutex> lock(engineMutex); engine = value; activeApplication = this; }
        unattended = command->display < BOOTSTRAPPER_DISPLAY_FULL;
        requested = command->action;
        runBy = command->relationType;
        if (command->commandLineScope != BOOTSTRAPPER_SCOPE_DEFAULT) { selectedScope = command->commandLineScope; explicitScope = true; }
        for (DWORD i = 0; i < m_BalInfoCommand.cUnknownArgs; ++i) {
            if (0 == lstrcmpW(m_BalInfoCommand.rgUnknownArgs[i], L"--check-engine")) checkEngine = true;
            else if (0 == lstrcmpW(m_BalInfoCommand.rgUnknownArgs[i], L"--check-plan")) checkPlan = true;
            else if (0 == lstrcmpW(m_BalInfoCommand.rgUnknownArgs[i], L"--check-plan-machine")) { checkPlan = true; selectedScope = BOOTSTRAPPER_SCOPE_PER_MACHINE; }
            else return E_INVALIDARG;
        }
        if (checkEngine && checkPlan) return E_INVALIDARG;
        Emit(8, checkEngine || checkPlan ? BOOTSTRAPPER_DISPLAY_NONE : command->display);
        if (unattended && !checkEngine) {
            // Burn requires a valid HWND even when no installer UI is displayed.
            const HRESULT owner = silentOwner.Start();
            if (FAILED(owner)) return owner;
            parentWindow = silentOwner.Window();
        }
        return S_OK;
    }
    STDMETHODIMP OnDestroy(BOOL) override {
        if (silentOwner.Window()) parentWindow = nullptr;
        return silentOwner.Stop();
    }
    STDMETHODIMP OnStartup() override {
        busy = true;
        const HRESULT status = m_pEngine->Detect(nullptr);
        if (FAILED(status)) Fail(status);
        return status;
    }
    STDMETHODIMP OnDetectPackageComplete(LPCWSTR id, HRESULT status, BOOTSTRAPPER_PACKAGE_STATE state, BOOL) override {
        if (FAILED(status)) return S_OK; // DetectComplete owns the final error.
        if (0 == lstrcmpW(id, L"FlinchMsi")) installed = state == BOOTSTRAPPER_PACKAGE_STATE_PRESENT;
        return S_OK;
    }
    STDMETHODIMP OnDetectRelatedBundle(LPCWSTR, BOOTSTRAPPER_RELATION_TYPE relation, LPCWSTR, BOOL perMachine, LPCWSTR version, BOOL, BOOL* cancel) override {
        *cancel = canceled.load() ? TRUE : FALSE;
        if (relation != BOOTSTRAPPER_RELATION_UPGRADE) return S_OK;
        wchar_t current[128] = {};
        SIZE_T length = ARRAYSIZE(current);
        HRESULT status = m_pEngine->GetVariableVersion(L"WixBundleVersion", current, &length);
        int comparison = 0;
        if (SUCCEEDED(status)) status = m_pEngine->CompareVersions(version, current, &comparison);
        if (FAILED(status)) relatedStatus = status;
        else if (comparison > 0) {
            const HRESULT newer = NewerRelatedBundleStatus(runBy, requested);
            if (FAILED(newer)) relatedStatus = newer;
        }
        else if (comparison < 0) upgrade = true;
        if (SUCCEEDED(status) && comparison <= 0) {
            const auto scope = perMachine ? BOOTSTRAPPER_SCOPE_PER_MACHINE : BOOTSTRAPPER_SCOPE_PER_USER;
            if (existingScope != BOOTSTRAPPER_SCOPE_DEFAULT && existingScope != scope) relatedStatus = InstalledScopesStatus(true, true);
            existingScope = scope;
        }
        return S_OK;
    }
    STDMETHODIMP OnDetectComplete(HRESULT status, BOOL) override {
        if (FAILED(status)) { Fail(status); return S_OK; }
        if (canceled.load()) { Fail(HRESULT_FROM_WIN32(ERROR_INSTALL_USEREXIT)); return S_OK; }
        if (selectingShortcuts.load()) {
            if (!installed) { Fail(HRESULT_FROM_WIN32(ERROR_UNKNOWN_PRODUCT)); return S_OK; }
            const HRESULT plan = m_pEngine->Plan(BOOTSTRAPPER_ACTION_MODIFY, selectedScope.load());
            if (FAILED(plan)) Fail(plan);
            return S_OK;
        }
        if (FAILED(relatedStatus)) { Fail(relatedStatus); return S_OK; }
        LONGLONG detectedScope = 0;
        const HRESULT scopeStatus = m_pEngine->GetVariableNumeric(L"WixBundleDetectedScope", &detectedScope);
        if (SUCCEEDED(scopeStatus) && (detectedScope == 1 || detectedScope == 2)) existingScope = static_cast<BOOTSTRAPPER_SCOPE>(detectedScope);
        if (installed && existingScope == BOOTSTRAPPER_SCOPE_DEFAULT) {
            wchar_t product[64] = {}; SIZE_T length = ARRAYSIZE(product);
            HRESULT productStatus = m_pEngine->GetVariableString(L"FlinchMsiProductCode", product, &length);
            if (FAILED(productStatus)) { Fail(productStatus); return S_OK; }
            DWORD sizeMachine = 0, sizeUser = 0;
            const UINT machine = MsiGetProductInfoExW(product, nullptr, MSIINSTALLCONTEXT_MACHINE, INSTALLPROPERTY_VERSIONSTRING, nullptr, &sizeMachine);
            const UINT user = MsiGetProductInfoExW(product, nullptr, MSIINSTALLCONTEXT_USERUNMANAGED, INSTALLPROPERTY_VERSIONSTRING, nullptr, &sizeUser);
            const bool hasMachine = machine == ERROR_SUCCESS || machine == ERROR_MORE_DATA;
            const bool hasUser = user == ERROR_SUCCESS || user == ERROR_MORE_DATA;
            const HRESULT scopes = InstalledScopesStatus(hasMachine, hasUser);
            if (FAILED(scopes)) { Fail(scopes); return S_OK; }
            existingScope = hasMachine ? BOOTSTRAPPER_SCOPE_PER_MACHINE : BOOTSTRAPPER_SCOPE_PER_USER;
        }
        if (existingScope != BOOTSTRAPPER_SCOPE_DEFAULT) {
            selectedScope = existingScope;
            scopeLocked = true;
            Emit(10, static_cast<int>(existingScope));
        }
        if (checkEngine) {
            const HRESULT logged = m_pEngine->Log(BOOTSTRAPPER_LOG_LEVEL_STANDARD, L"Flinch engine check: detection only; no plan or apply.");
            return m_pEngine->Quit(FAILED(logged) ? static_cast<DWORD>(logged) : 0);
        }
        if (checkPlan) {
            activeAction = BOOTSTRAPPER_ACTION_INSTALL;
            const HRESULT plan = m_pEngine->Plan(BOOTSTRAPPER_ACTION_INSTALL, selectedScope.load());
            if (FAILED(plan)) Fail(plan);
            return S_OK;
        }
        busy = false;
        Emit(1, installed ? 1 : upgrade ? 2 : 0);
        if (unattended) {
            if (requested != BOOTSTRAPPER_ACTION_INSTALL && requested != BOOTSTRAPPER_ACTION_REPAIR && requested != BOOTSTRAPPER_ACTION_UNINSTALL) {
                Fail(E_INVALIDARG); return S_OK;
            }
            busy = true;
            operationStarted = true;
            Emit(2, requested);
            activeAction = requested;
            const HRESULT plan = m_pEngine->Plan(requested, UnattendedPlanScope(scopeLocked.load(), explicitScope, selectedScope.load()));
            if (FAILED(plan)) Fail(plan);
        }
        return S_OK;
    }
    STDMETHODIMP OnPlanPackageBegin(LPCWSTR id, BOOTSTRAPPER_PACKAGE_STATE state, BOOL, BOOTSTRAPPER_PACKAGE_CONDITION_RESULT, BOOTSTRAPPER_PACKAGE_CONDITION_RESULT, BOOTSTRAPPER_REQUEST_STATE, BOOTSTRAPPER_CACHE_TYPE, BOOTSTRAPPER_REQUEST_STATE* request, BOOTSTRAPPER_CACHE_TYPE* cache, BOOL* cancel) override {
        *cancel = canceled.load() ? TRUE : FALSE;
        if (0 == lstrcmpW(id, L"VisualCppRuntime") && state == BOOTSTRAPPER_PACKAGE_STATE_PRESENT) {
            // The prerequisite stage has already satisfied this shared dependency.
            // Do not trigger machine-wide caching/elevation for a per-user app install.
            *request = BOOTSTRAPPER_REQUEST_STATE_NONE;
            *cache = BOOTSTRAPPER_CACHE_TYPE_REMOVE;
        }
        return S_OK;
    }
    STDMETHODIMP OnPlanMsiFeature(LPCWSTR package, LPCWSTR feature, BOOTSTRAPPER_FEATURE_STATE, BOOTSTRAPPER_FEATURE_STATE* request, BOOL* cancel) override {
        *cancel = canceled.load() ? TRUE : FALSE;
        if (0 != lstrcmpW(package, L"FlinchMsi")) return S_OK;
        // Only a fresh install or the explicit post-install choice changes features.
        if ((!installed && activeAction.load() == BOOTSTRAPPER_ACTION_INSTALL) || selectingShortcuts.load()) {
            if (0 == lstrcmpW(feature, L"Application")) *request = BOOTSTRAPPER_FEATURE_STATE_LOCAL;
            else if (0 == lstrcmpW(feature, L"StartMenuFeature")) *request = wantMenu.load() ? BOOTSTRAPPER_FEATURE_STATE_LOCAL : BOOTSTRAPPER_FEATURE_STATE_ABSENT;
            else if (0 == lstrcmpW(feature, L"DesktopFeature")) *request = wantDesktop.load() ? BOOTSTRAPPER_FEATURE_STATE_LOCAL : BOOTSTRAPPER_FEATURE_STATE_ABSENT;
            if (0 == lstrcmpW(feature, L"StartMenuFeature")) m_pEngine->Log(BOOTSTRAPPER_LOG_LEVEL_STANDARD, wantMenu.load() ? L"Flinch shortcut plan: Start menu selected." : L"Flinch shortcut plan: Start menu absent.");
            if (0 == lstrcmpW(feature, L"DesktopFeature")) m_pEngine->Log(BOOTSTRAPPER_LOG_LEVEL_STANDARD, wantDesktop.load() ? L"Flinch shortcut plan: Desktop selected." : L"Flinch shortcut plan: Desktop absent.");
        }
        return S_OK;
    }
    STDMETHODIMP OnPlanComplete(HRESULT status) override {
        if (FAILED(status)) { Fail(status); return S_OK; }
        // Cancellation can arrive after the last cancellable planning event.
        if (canceled.load()) { Fail(HRESULT_FROM_WIN32(ERROR_INSTALL_USEREXIT)); return S_OK; }
        const HWND parent = parentWindow.load();
        if (!parent || !IsWindow(parent)) { Fail(HRESULT_FROM_WIN32(ERROR_INVALID_WINDOW_HANDLE)); return S_OK; }
        if (checkPlan) {
            const HRESULT logged = m_pEngine->Log(BOOTSTRAPPER_LOG_LEVEL_STANDARD, L"Flinch plan check: valid parent HWND; no apply.");
            return m_pEngine->Quit(FAILED(logged) ? static_cast<DWORD>(logged) : 0);
        }
        Emit(3);
        m_pEngine->Log(BOOTSTRAPPER_LOG_LEVEL_STANDARD, L"Flinch Apply: validated parent HWND.");
        const HRESULT apply = m_pEngine->Apply(parent);
        if (FAILED(apply)) Fail(apply);
        return S_OK;
    }
    STDMETHODIMP OnProgress(DWORD, DWORD overall, BOOL* cancel) override {
        *cancel |= !IsRollingBack() && canceled.load() ? TRUE : FALSE;
        Emit(4, static_cast<int>(overall));
        return S_OK;
    }
    STDMETHODIMP OnApplyBegin(DWORD, BOOL* cancel) override {
        *cancel = canceled.load() ? TRUE : FALSE;
        return S_OK;
    }
    // Cache work is never rollback. Check our request explicitly because the
    // SDK can retain the previous transaction's rollback state until execution.
    STDMETHODIMP OnCacheBegin(BOOL* cancel) override {
        *cancel |= canceled.load() ? TRUE : FALSE;
        return S_OK;
    }
    STDMETHODIMP OnCacheAcquireProgress(LPCWSTR id, LPCWSTR payload, DWORD64 progress, DWORD64 total, DWORD overall, BOOL* cancel) override {
        const HRESULT status = CBootstrapperApplicationBase::OnCacheAcquireProgress(id, payload, progress, total, overall, cancel);
        *cancel |= canceled.load() ? TRUE : FALSE;
        return status;
    }
    STDMETHODIMP OnCacheVerifyProgress(LPCWSTR id, LPCWSTR payload, DWORD64 progress, DWORD64 total, DWORD overall, BOOTSTRAPPER_CACHE_VERIFY_STEP step, BOOL* cancel) override {
        const HRESULT status = CBootstrapperApplicationBase::OnCacheVerifyProgress(id, payload, progress, total, overall, step, cancel);
        *cancel |= canceled.load() ? TRUE : FALSE;
        return status;
    }
    STDMETHODIMP OnElevateComplete(HRESULT status) override {
        // Queue restoration on Slint's owner thread after either accepting or
        // declining UAC. Progress events must not repeatedly steal focus.
        Emit(11, static_cast<int>(status));
        return S_OK;
    }
    STDMETHODIMP OnExecutePackageBegin(LPCWSTR id, BOOL execute, BOOTSTRAPPER_ACTION_STATE action, INSTALLUILEVEL level, BOOL disableExternalUi, BOOL* cancel) override {
        if (execute) filesInUseShutdownRequested = false;
        Emit(9, 0 == lstrcmpW(id, L"VisualCppRuntime") ? 1 : 2);
        const HRESULT status = CBootstrapperApplicationBase::OnExecutePackageBegin(id, execute, action, level, disableExternalUi, cancel);
        if (execute && canceled.load()) *cancel = TRUE;
        return status;
    }
    STDMETHODIMP OnExecuteFilesInUse(LPCWSTR id, DWORD, LPCWSTR*, int recommendation, BOOTSTRAPPER_FILES_IN_USE_TYPE source, int* result) override {
        if (IsRollingBack()) { *result = recommendation; return S_OK; }
        if (canceled.load() || CheckCanceled()) { *result = IDCANCEL; return S_OK; }
        if (0 != lstrcmpW(id, L"FlinchMsi")) { *result = recommendation; return S_OK; }
        // Restart Manager identifies owners of the actual MSI payload paths.
        // IDIGNORE would leave Flinch running and defer removal until reboot.
        // Never retry legacy locks forever or kill processes by executable name.
        const bool close = source == BOOTSTRAPPER_FILES_IN_USE_TYPE_MSI_RM && !filesInUseShutdownRequested;
        *result = close ? IDOK : -1;
        if (close) filesInUseShutdownRequested = true;
        if (m_pEngine) m_pEngine->Log(BOOTSTRAPPER_LOG_LEVEL_STANDARD,
            *result == IDOK ? L"Closing applications using the installed Flinch files through Windows Restart Manager."
                           : L"Flinch files could not be closed; close Flinch and retry instead of deferring removal until reboot.");
        return S_OK;
    }
    STDMETHODIMP OnError(BOOTSTRAPPER_ERROR_TYPE, LPCWSTR, DWORD code, LPCWSTR message, DWORD, DWORD, LPCWSTR*, int recommendation, int* result) override {
        m_pEngine->Log(BOOTSTRAPPER_LOG_LEVEL_ERROR, message);
        resultCode = static_cast<int>(HRESULT_FROM_WIN32(code));
        // Preserve Burn's recommendation; never accept an error as success or retry forever.
        *result = !IsRollingBack() && canceled.load() ? IDCANCEL : recommendation;
        return S_OK;
    }
    STDMETHODIMP OnApplyComplete(HRESULT status, BOOTSTRAPPER_APPLY_RESTART restart, BOOTSTRAPPER_APPLYCOMPLETE_ACTION, BOOTSTRAPPER_APPLYCOMPLETE_ACTION* action) override {
        busy = false;
        if (restart != BOOTSTRAPPER_APPLY_RESTART_NONE) restartRequired = true;
        const bool reboot = restartRequired.load();
        resultCode = FAILED(status) ? static_cast<int>(status) : reboot ? ERROR_SUCCESS_REBOOT_REQUIRED : 0;
        *action = BOOTSTRAPPER_APPLYCOMPLETE_ACTION_NONE; // Never restart Windows automatically.
        Emit(5, static_cast<int>(status), reboot ? 1 : 0);
        if (unattended) m_pEngine->Quit(static_cast<DWORD>(resultCode.load()));
        return S_OK;
    }
    STDMETHODIMP OnShutdown(BOOTSTRAPPER_SHUTDOWN_ACTION* action) override {
        *action = BOOTSTRAPPER_SHUTDOWN_ACTION_NONE;
        return S_OK;
    }
};

extern "C" __declspec(dllexport) int __cdecl flinch_burn_run(Notify notify, void* context) {
    if (!notify || !context) return E_INVALIDARG;
    auto* application = new (std::nothrow) FlinchApplication(notify, context);
    if (!application) return E_OUTOFMEMORY;
    const HRESULT status = BootstrapperApplicationRun(application);
    { std::lock_guard<std::mutex> lock(engineMutex); engine = nullptr; activeApplication = nullptr; }
    parentWindow = nullptr;
    application->Release();
    return static_cast<int>(status);
}

extern "C" __declspec(dllexport) int __cdecl flinch_burn_set_parent(HWND parent) {
    if (!parent || !IsWindow(parent)) return HRESULT_FROM_WIN32(ERROR_INVALID_WINDOW_HANDLE);
    parentWindow = parent;
    return S_OK;
}

extern "C" __declspec(dllexport) int __cdecl flinch_burn_command(int operation) {
    IBootstrapperEngine* current = nullptr;
    FlinchApplication* application = nullptr;
    {
        std::lock_guard<std::mutex> lock(engineMutex);
        current = engine;
        if (current) current->AddRef();
        application = activeApplication;
        if (application) application->AddRef();
    }
    if (!current || !application) {
        if (current) current->Release();
        if (application) application->Release();
        return E_UNEXPECTED;
    }
    HRESULT status = S_OK;
    if (operation == 0) {
        if (busy.load()) status = HRESULT_FROM_WIN32(ERROR_BUSY);
        else status = current->Quit(static_cast<DWORD>(resultCode.load()));
    } else if (operation == 9) {
        application->RequestCancel();
        if (!busy.load()) status = current->Quit(operationStarted.load() ? static_cast<DWORD>(resultCode.load()) : ERROR_INSTALL_USEREXIT);
    } else if (operation == BOOTSTRAPPER_ACTION_INSTALL || operation == BOOTSTRAPPER_ACTION_MODIFY || operation == BOOTSTRAPPER_ACTION_REPAIR || operation == BOOTSTRAPPER_ACTION_UNINSTALL) {
        if (busy.exchange(true)) status = HRESULT_FROM_WIN32(ERROR_BUSY);
        else {
            application->ResetCancellation();
            operationStarted = true;
            activeAction = static_cast<BOOTSTRAPPER_ACTION>(operation);
            selectingShortcuts = operation == BOOTSTRAPPER_ACTION_MODIFY;
            // Apply changes detection state; refresh it before the second transaction.
            status = selectingShortcuts.load() ? current->Detect(parentWindow.load()) : current->Plan(static_cast<BOOTSTRAPPER_ACTION>(operation), selectedScope.load());
            if (FAILED(status)) busy = false;
        }
    } else status = E_INVALIDARG;
    if (FAILED(status)) resultCode = static_cast<int>(status);
    current->Release(); // No engine callbacks are invoked while holding engineMutex.
    application->Release();
    return static_cast<int>(status);
}

extern "C" __declspec(dllexport) int __cdecl flinch_burn_set_options(int scope, LPCWSTR folder, int startMenu, int desktop) {
    if ((scope != BOOTSTRAPPER_SCOPE_PER_MACHINE && scope != BOOTSTRAPPER_SCOPE_PER_USER) || !folder || !*folder || (startMenu != 0 && startMenu != 1) || (desktop != 0 && desktop != 1)) return E_INVALIDARG;
    if (busy.load()) return HRESULT_FROM_WIN32(ERROR_BUSY);
    if (scopeLocked.load() && scope != selectedScope.load()) return HRESULT_FROM_WIN32(ERROR_PRODUCT_VERSION);
    IBootstrapperEngine* current = nullptr;
    { std::lock_guard<std::mutex> lock(engineMutex); current = engine; if (current) current->AddRef(); }
    if (!current) return E_UNEXPECTED;
    HRESULT status = current->SetVariableString(L"InstallFolder", folder, FALSE);
    if (SUCCEEDED(status)) status = current->SetVariableNumeric(L"StartMenu", startMenu);
    if (SUCCEEDED(status)) status = current->SetVariableNumeric(L"DesktopShortcut", desktop);
    if (SUCCEEDED(status)) {
        selectedScope = static_cast<BOOTSTRAPPER_SCOPE>(scope);
        wantMenu = startMenu != 0;
        wantDesktop = desktop != 0;
    }
    current->Release();
    return status;
}
