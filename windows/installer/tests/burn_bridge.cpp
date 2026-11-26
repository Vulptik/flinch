// Quiet callback regression: no engine, windows, elevation or MSI transaction.
#include "../burn_bridge.cpp"
#include <cstdio>

struct Events {
    int count = 0;
    int kind = 0;
    int status = 0;
};

static void __cdecl Collect(void* context, int kind, int status, int) {
    auto* events = static_cast<Events*>(context);
    ++events->count;
    events->kind = kind;
    events->status = status;
}

struct OwnerProbe {
    HWND window = nullptr;
    LRESULT delivered = 0;
};

// Burn's engine reaches the owner from another thread (and process) while the BA thread is blocked.
static DWORD WINAPI SendToOwner(void* value) {
    auto* probe = static_cast<OwnerProbe*>(value);
    DWORD_PTR result = 0;
    probe->delivered = SendMessageTimeoutW(probe->window, WM_NULL, 0, 0, SMTO_ABORTIFHUNG | SMTO_BLOCK, 2000, &result);
    return 0;
}

int main() {
    {
        SilentOwner owner;
        if (FAILED(owner.Start()) || !IsWindow(owner.Window())) {
            std::fputs("The quiet-operation owner window could not start.\n", stderr);
            return 1;
        }
        OwnerProbe probe{owner.Window(), 0};
        const HANDLE sender = CreateThread(nullptr, 0, SendToOwner, &probe, 0, nullptr);
        // This thread now blocks without pumping messages, like the Burn callback thread.
        if (!sender || WaitForSingleObject(sender, 10000) != WAIT_OBJECT_0 || !probe.delivered) {
            std::fputs("The quiet-operation owner window stopped answering while its creator was blocked.\n", stderr);
            return 1;
        }
        CloseHandle(sender);
        const HWND window = owner.Window();
        if (FAILED(owner.Stop()) || IsWindow(window) || owner.Window() || FAILED(owner.Stop())) {
            std::fputs("The quiet-operation owner window did not stop within its bound.\n", stderr);
            return 1;
        }
    }
    // A newer bundle removing this one during its upgrade must not be refused as a downgrade;
    // every other operation beside a newer installed version still is.
    if (FAILED(NewerRelatedBundleStatus(BOOTSTRAPPER_RELATION_UPGRADE, BOOTSTRAPPER_ACTION_UNINSTALL))) {
        std::fputs("An upgrading newer bundle could not remove the older bundle.\n", stderr);
        return 1;
    }
    const HRESULT downgrade = HRESULT_FROM_WIN32(ERROR_PRODUCT_VERSION);
    if (NewerRelatedBundleStatus(BOOTSTRAPPER_RELATION_NONE, BOOTSTRAPPER_ACTION_INSTALL) != downgrade ||
        NewerRelatedBundleStatus(BOOTSTRAPPER_RELATION_NONE, BOOTSTRAPPER_ACTION_UNINSTALL) != downgrade ||
        NewerRelatedBundleStatus(BOOTSTRAPPER_RELATION_UPGRADE, BOOTSTRAPPER_ACTION_INSTALL) != downgrade ||
        NewerRelatedBundleStatus(BOOTSTRAPPER_RELATION_UPGRADE, BOOTSTRAPPER_ACTION_REPAIR) != downgrade) {
        std::fputs("An older bundle accepted an operation beside a newer installed version.\n", stderr);
        return 1;
    }
    // The UI maps these exact codes to plain-language copy (flinch_installer::failure_copy).
    if (downgrade != static_cast<HRESULT>(0x80070666) ||
        InstalledScopesStatus(true, true) != static_cast<HRESULT>(0x800700B7) ||
        InstalledScopesStatus(false, false) != HRESULT_FROM_WIN32(ERROR_UNKNOWN_PRODUCT) ||
        FAILED(InstalledScopesStatus(true, false)) || FAILED(InstalledScopesStatus(false, true))) {
        std::fputs("Detection failures lost the codes the installer UI explains.\n", stderr);
        return 1;
    }
    // A quiet fresh install leaves the scope to Burn so -permachine applies; installed or chosen scopes stay.
    if (UnattendedPlanScope(false, false, BOOTSTRAPPER_SCOPE_PER_USER) != BOOTSTRAPPER_SCOPE_DEFAULT ||
        UnattendedPlanScope(true, false, BOOTSTRAPPER_SCOPE_PER_MACHINE) != BOOTSTRAPPER_SCOPE_PER_MACHINE ||
        UnattendedPlanScope(true, false, BOOTSTRAPPER_SCOPE_PER_USER) != BOOTSTRAPPER_SCOPE_PER_USER ||
        UnattendedPlanScope(false, true, BOOTSTRAPPER_SCOPE_PER_MACHINE) != BOOTSTRAPPER_SCOPE_PER_MACHINE) {
        std::fputs("A quiet install overrode the command-line scope or an installed scope.\n", stderr);
        return 1;
    }
    Events events;
    FlinchApplication application(Collect, &events);
    const HRESULT outcomes[] = {S_OK, HRESULT_FROM_WIN32(ERROR_CANCELLED), E_FAIL};
    for (const HRESULT status : outcomes) {
        events = Events{};
        if (application.OnElevateComplete(status) != S_OK || events.count != 1 ||
            events.kind != 11 || events.status != static_cast<int>(status)) {
            std::fputs("Elevation completion did not request installer activation.\n", stderr);
            return 1;
        }
    }
    // The inherited SDK callback leaves IDIGNORE untouched: the old bug.
    int locked = IDIGNORE;
    application.CBootstrapperApplicationBase::OnExecuteFilesInUse(L"FlinchMsi", 0, nullptr, IDIGNORE, BOOTSTRAPPER_FILES_IN_USE_TYPE_MSI_RM, &locked);
    if (locked != IDIGNORE) return 1;
    application.OnExecuteFilesInUse(L"FlinchMsi", 0, nullptr, IDIGNORE, BOOTSTRAPPER_FILES_IN_USE_TYPE_MSI_RM, &locked);
    if (locked != IDOK) {
        std::fputs("Restart Manager was not allowed to close the installed app.\n", stderr);
        return 1;
    }
    application.OnExecuteFilesInUse(L"FlinchMsi", 0, nullptr, IDIGNORE, BOOTSTRAPPER_FILES_IN_USE_TYPE_MSI_RM, &locked);
    if (locked != -1) {
        std::fputs("Repeated files-in-use callbacks must fail rather than loop or defer deletion.\n", stderr);
        return 1;
    }
    application.OnExecuteFilesInUse(L"FlinchMsi", 0, nullptr, IDIGNORE, BOOTSTRAPPER_FILES_IN_USE_TYPE_MSI, &locked);
    if (locked != -1) return 1;
    application.OnExecuteFilesInUse(L"VisualCppRuntime", 0, nullptr, IDIGNORE, BOOTSTRAPPER_FILES_IN_USE_TYPE_MSI_RM, &locked);
    if (locked != IDIGNORE) return 1;
    application.RequestCancel();
    application.OnExecuteFilesInUse(L"FlinchMsi", 0, nullptr, IDOK, BOOTSTRAPPER_FILES_IN_USE_TYPE_MSI_RM, &locked);
    if (locked != IDCANCEL) return 1;
    BOOL cancel = FALSE;
    application.OnExecuteProgress(L"FlinchMsi", 25, 25, &cancel);
    if (!cancel) {
        std::fputs("MSI execution ignored the user's cancellation.\n", stderr);
        return 1;
    }
    cancel = FALSE;
    application.OnCacheAcquireProgress(L"FlinchMsi", nullptr, 1, 10, 10, &cancel);
    if (!cancel) {
        std::fputs("Cache acquisition ignored cancellation.\n", stderr);
        return 1;
    }
    cancel = FALSE;
    application.OnCacheVerifyBegin(L"FlinchMsi", L"payload", &cancel);
    if (!cancel) {
        std::fputs("Cache verification ignored cancellation.\n", stderr);
        return 1;
    }
    int response = IDOK;
    application.OnExecuteMsiMessage(L"FlinchMsi", INSTALLMESSAGE_ACTIONSTART, 0, L"", 0, nullptr, IDOK, &response);
    if (response != IDCANCEL) {
        std::fputs("MSI messages ignored cancellation.\n", stderr);
        return 1;
    }
    // Cancellation after the final planning callback must never start Apply.
    events = Events{};
    busy = true;
    application.OnPlanComplete(S_OK);
    if (events.kind != 6 || events.status != static_cast<int>(HRESULT_FROM_WIN32(ERROR_INSTALL_USEREXIT)) || busy.load()) {
        std::fputs("Canceled planning did not terminate before Apply.\n", stderr);
        return 1;
    }
    events = Events{};
    application.OnDetectComplete(S_OK, FALSE);
    if (events.kind != 6 || events.status != static_cast<int>(HRESULT_FROM_WIN32(ERROR_INSTALL_USEREXIT))) {
        std::fputs("Canceled detection continued into planning.\n", stderr);
        return 1;
    }
    cancel = FALSE;
    application.OnExecutePackageBegin(L"FlinchMsi", FALSE, BOOTSTRAPPER_ACTION_STATE_UNINSTALL, INSTALLUILEVEL_NONE, FALSE, &cancel);
    application.OnExecuteProgress(L"FlinchMsi", 50, 50, &cancel);
    application.OnProgress(50, 50, &cancel);
    response = IDOK;
    application.OnExecuteMsiMessage(L"FlinchMsi", INSTALLMESSAGE_ACTIONSTART, 0, L"", 0, nullptr, IDOK, &response);
    if (cancel || response != IDOK) {
        std::fputs("Cancellation interrupted rollback.\n", stderr);
        return 1;
    }
    application.OnExecuteFilesInUse(L"FlinchMsi", 0, nullptr, IDOK, BOOTSTRAPPER_FILES_IN_USE_TYPE_MSI_RM, &locked);
    if (locked != IDOK) return 1; // Never cancel rollback.
    BOOTSTRAPPER_APPLYCOMPLETE_ACTION action = BOOTSTRAPPER_APPLYCOMPLETE_ACTION_NONE;
    busy = true;
    application.OnApplyComplete(HRESULT_FROM_WIN32(ERROR_INSTALL_USEREXIT), BOOTSTRAPPER_APPLY_RESTART_NONE, action, &action);
    if (busy.load() || events.kind != 5 || events.status != static_cast<int>(HRESULT_FROM_WIN32(ERROR_INSTALL_USEREXIT))) {
        std::fputs("Cancellation did not finish after rollback.\n", stderr);
        return 1;
    }
    application.ResetCancellation();
    cancel = FALSE;
    application.OnDetectBegin(FALSE, BOOTSTRAPPER_REGISTRATION_TYPE_NONE, 1, &cancel);
    if (cancel || canceled.load()) {
        std::fputs("Retry inherited cancellation from the previous operation.\n", stderr);
        return 1;
    }
    application.RequestCancel();
    cancel = FALSE;
    application.OnCacheBegin(&cancel);
    if (!cancel) {
        std::fputs("A retry's cache stage inherited stale rollback state.\n", stderr);
        return 1;
    }
    application.ResetCancellation();
    cancel = FALSE;
    application.OnExecutePackageBegin(L"FlinchMsi", TRUE, BOOTSTRAPPER_ACTION_STATE_UNINSTALL, INSTALLUILEVEL_NONE, FALSE, &cancel);
    application.OnExecuteFilesInUse(L"FlinchMsi", 0, nullptr, IDIGNORE, BOOTSTRAPPER_FILES_IN_USE_TYPE_MSI_RM, &locked);
    if (locked != IDOK) {
        std::fputs("A new execution did not reset its bounded shutdown attempt.\n", stderr);
        return 1;
    }
    std::puts("Cancellation reaches cache and MSI, stops detection/planning, preserves rollback and resets for retry.");
    std::puts("Restart Manager closes file owners once; cancellation, rollback and unreleased locks stay explicit.");
    std::puts("Elevation success, cancellation and failure each request one UI activation; no OS interaction.");
    std::puts("A newer bundle's related upgrade can remove this bundle; other operations beside a newer version fail.");
    std::puts("Downgrade and dual-scope detection failures keep the codes the UI explains.");
    std::puts("Quiet fresh installs leave the scope to Burn's command line; installed scopes stay.");
    std::puts("The quiet-operation owner window answers messages while the Burn thread blocks and stops within its bound.");
    return 0;
}
