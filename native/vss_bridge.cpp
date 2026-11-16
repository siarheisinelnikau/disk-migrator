// GPLv3. Copyright (c) 2026 siarheisinelnikau.
// This SDK bridge does not run during build or tests. Rust owns operation ordering.
#include <windows.h>
#include <vss.h>
#include <vswriter.h>
#include <vsbackup.h>
#include <vsserror.h>
#include <new>
#include <string>
#include <vector>

using Cancelled = unsigned int (__cdecl *)(void*);
struct Session {
    IVssBackupComponents* backup = nullptr;
    bool com = false;
    bool metadata = false;
    bool initialized = false;
    bool completed = false;
    GUID set = GUID_NULL;
    std::vector<GUID> snapshots;
    ~Session() {
        if (backup) {
            // Failed requesters must not leave writers in a waiting backup session.
            if (initialized && !completed) backup->AbortBackup();
            if (set != GUID_NULL) {
                LONG deleted = 0; GUID notDeleted = GUID_NULL;
                backup->DeleteSnapshots(set, VSS_OBJECT_SNAPSHOT_SET, TRUE, &deleted, &notDeleted);
            }
            if (metadata) backup->FreeWriterMetadata();
            backup->Release();
        }
        if (com) CoUninitialize();
    }
};
static HRESULT await_async(HRESULT call, IVssAsync* task, Cancelled cancelled, void* context) {
    if (FAILED(call)) { if (task) task->Release(); return call; }
    if (!task) return E_UNEXPECTED;
    const ULONGLONG end = GetTickCount64() + 180000;
    HRESULT result = VSS_S_ASYNC_PENDING;
    while (GetTickCount64() < end) {
        if (cancelled && cancelled(context)) { task->Cancel(); task->Release(); return HRESULT_FROM_WIN32(ERROR_CANCELLED); }
        HRESULT hr = task->QueryStatus(&result, nullptr);
        if (FAILED(hr)) { task->Release(); return hr; }
        if (result == VSS_S_ASYNC_FINISHED || result == S_OK) { task->Release(); return S_OK; }
        if (result != VSS_S_ASYNC_PENDING) { task->Release(); return FAILED(result) ? result : E_ABORT; }
        hr = task->Wait(100);
        if (FAILED(hr)) { task->Release(); return hr; }
    }
    task->Cancel(); task->Release(); return HRESULT_FROM_WIN32(ERROR_TIMEOUT);
}
extern "C" {
HRESULT __cdecl dm_vss_create(void** output) {
    if (!output) return E_POINTER;
    *output = nullptr;
    Session* s = new (std::nothrow) Session(); if (!s) return E_OUTOFMEMORY;
    HRESULT hr = CoInitializeEx(nullptr, COINIT_MULTITHREADED);
    if (FAILED(hr)) { delete s; return hr; }
    s->com = true;
    hr = CoInitializeSecurity(nullptr, -1, nullptr, nullptr, RPC_C_AUTHN_LEVEL_PKT_PRIVACY, RPC_C_IMP_LEVEL_IMPERSONATE, nullptr, EOAC_NONE, nullptr);
    if (FAILED(hr) && hr != RPC_E_TOO_LATE) { delete s; return hr; }
    hr = CreateVssBackupComponents(&s->backup);
    if (FAILED(hr)) { delete s; return hr; }
    *output = s; return S_OK;
}
HRESULT __cdecl dm_vss_initialize(void* raw) {
    auto s = static_cast<Session*>(raw); if (!s || !s->backup) return E_POINTER;
    HRESULT hr = s->backup->InitializeForBackup(); if (FAILED(hr)) return hr;
    hr = s->backup->SetContext(VSS_CTX_BACKUP); if (FAILED(hr)) return hr;
    hr = s->backup->SetBackupState(false, true, VSS_BT_COPY, false);
    if (SUCCEEDED(hr)) s->initialized = true;
    return hr;
}
HRESULT __cdecl dm_vss_metadata(void* raw, Cancelled cancelled, void* context) {
    auto s = static_cast<Session*>(raw); IVssAsync* task = nullptr;
    HRESULT hr = s->backup->GatherWriterMetadata(&task);
    hr = await_async(hr, task, cancelled, context); if (SUCCEEDED(hr)) s->metadata = true; return hr;
}
HRESULT __cdecl dm_vss_begin(void* raw, wchar_t* output, unsigned int capacity) {
    auto s = static_cast<Session*>(raw); if (capacity < 39) return E_INVALIDARG;
    HRESULT hr = s->backup->StartSnapshotSet(&s->set); if (FAILED(hr)) return hr;
    return StringFromGUID2(s->set, output, capacity) ? S_OK : E_FAIL;
}
HRESULT __cdecl dm_vss_add(void* raw, const wchar_t* volume, wchar_t* output, unsigned int capacity) {
    auto s = static_cast<Session*>(raw); if (!volume || capacity < 39) return E_INVALIDARG;
    BOOL supported = FALSE; HRESULT hr = s->backup->IsVolumeSupported(GUID_NULL, const_cast<wchar_t*>(volume), &supported);
    if (FAILED(hr)) return hr; if (!supported) return VSS_E_VOLUME_NOT_SUPPORTED;
    GUID id; hr = s->backup->AddToSnapshotSet(const_cast<wchar_t*>(volume), GUID_NULL, &id); if (FAILED(hr)) return hr;
    try { s->snapshots.push_back(id); } catch (...) { return E_OUTOFMEMORY; }
    return StringFromGUID2(id, output, capacity) ? S_OK : E_FAIL;
}
HRESULT __cdecl dm_vss_prepare(void* raw, Cancelled cancelled, void* context) {
    auto s = static_cast<Session*>(raw); IVssAsync* task = nullptr;
    HRESULT hr = s->backup->PrepareForBackup(&task); return await_async(hr, task, cancelled, context);
}
HRESULT __cdecl dm_vss_snapshot(void* raw, Cancelled cancelled, void* context) {
    auto s = static_cast<Session*>(raw); IVssAsync* task = nullptr;
    HRESULT hr = s->backup->DoSnapshotSet(&task); return await_async(hr, task, cancelled, context);
}
struct WriterResult { wchar_t name[256]; wchar_t id[40]; unsigned int state; int failure; };
HRESULT __cdecl dm_vss_writers(void* raw, Cancelled cancelled, void* context, WriterResult* output, unsigned int capacity, unsigned int* count) {
    auto s = static_cast<Session*>(raw); if (!output || !count) return E_POINTER;
    IVssAsync* task = nullptr; HRESULT hr = s->backup->GatherWriterStatus(&task);
    hr = await_async(hr, task, cancelled, context); if (FAILED(hr)) return hr;
    UINT writers = 0; hr = s->backup->GetWriterStatusCount(&writers);
    if (FAILED(hr) || writers > capacity) { s->backup->FreeWriterStatus(); return FAILED(hr) ? hr : HRESULT_FROM_WIN32(ERROR_INSUFFICIENT_BUFFER); }
    for (UINT i = 0; i < writers; ++i) {
        VSS_ID instance = GUID_NULL, writer = GUID_NULL; BSTR name = nullptr;
        VSS_WRITER_STATE state; HRESULT failure = S_OK;
        hr = s->backup->GetWriterStatus(i, &instance, &writer, &name, &state, &failure);
        if (FAILED(hr)) { if (name) SysFreeString(name); s->backup->FreeWriterStatus(); return hr; }
        output[i] = {}; if (name) { wcsncpy_s(output[i].name, name, _TRUNCATE); SysFreeString(name); }
        output[i].state = state; output[i].failure = failure;
        StringFromGUID2(writer, output[i].id, 40);
    }
    *count = writers; s->backup->FreeWriterStatus(); return S_OK;
}
HRESULT __cdecl dm_vss_path(void* raw, const wchar_t* snapshot, wchar_t* output, unsigned int capacity) {
    auto s = static_cast<Session*>(raw); GUID id; HRESULT hr = CLSIDFromString(snapshot, &id); if (FAILED(hr)) return hr;
    VSS_SNAPSHOT_PROP property = {}; hr = s->backup->GetSnapshotProperties(id, &property); if (FAILED(hr)) return hr;
    if (property.m_SnapshotSetId != s->set || property.m_eStatus != VSS_SS_CREATED || !property.m_pwszSnapshotDeviceObject || wcslen(property.m_pwszSnapshotDeviceObject) + 1 > capacity) {
        VssFreeSnapshotProperties(&property); return E_UNEXPECTED;
    }
    wcscpy_s(output, capacity, property.m_pwszSnapshotDeviceObject); VssFreeSnapshotProperties(&property); return S_OK;
}
HRESULT __cdecl dm_vss_complete(void* raw, Cancelled cancelled, void* context) {
    auto s = static_cast<Session*>(raw); IVssAsync* task = nullptr;
    HRESULT hr = s->backup->BackupComplete(&task); hr = await_async(hr, task, cancelled, context);
    if (SUCCEEDED(hr)) s->completed = true; return hr;
}
HRESULT __cdecl dm_vss_abort(void* raw) { auto s = static_cast<Session*>(raw); return s->initialized && !s->completed ? s->backup->AbortBackup() : S_OK; }
HRESULT __cdecl dm_vss_delete(void* raw) {
    auto s = static_cast<Session*>(raw); if (s->set == GUID_NULL) return S_OK;
    LONG deleted = 0; GUID notDeleted = GUID_NULL;
    HRESULT hr = s->backup->DeleteSnapshots(s->set, VSS_OBJECT_SNAPSHOT_SET, TRUE, &deleted, &notDeleted);
    if (hr == VSS_E_OBJECT_NOT_FOUND) hr = S_OK;
    if (SUCCEEDED(hr)) s->set = GUID_NULL; return hr;
}
void __cdecl dm_vss_destroy(void* raw) { delete static_cast<Session*>(raw); }
}
