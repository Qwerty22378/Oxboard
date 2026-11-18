/*++

Copyright (c) Microsoft Corporation.  All rights reserved.

Module Name:

    Device.cpp - Device handling events for example driver.

Abstract:

   This file contains the device entry points and callbacks.

Environment:

    Kernel-mode Driver Framework

--*/

#include <wdm.h>
#include <windef.h>
#include "private.h"
#include "public.h"
#include <devguid.h>
#include <ks.h>
#include <mmsystem.h>
#include <ksmedia.h>
#include "streamengine.h"
#include "DriverSettings.h"

// ---- Spike 3 (dynamic endpoints) ----
#define SPIKE_FIRST_DELAY_S  20
static WDFTIMER g_SpikeTimer;
NTSTATUS Spike_Init(_In_ WDFDEVICE Parent);

#ifndef __INTELLISENSE__
#include "device.tmh"
#endif

UNICODE_STRING g_RegistryPath = { 0 };      // This is used to store the registry settings path for the driver

ULONG DeviceDriverTag = DRIVER_TAG;

ULONG IdleTimeoutMsec = IDLE_TIMEOUT_MSEC;

__drv_requiresIRQL(PASSIVE_LEVEL)
PAGED_CODE_SEG
NTSTATUS
CopyRegistrySettingsPath(
    _In_ PUNICODE_STRING RegistryPath
)
/*++

Routine Description:

Copies the following registry path to a global variable.

\REGISTRY\MACHINE\SYSTEM\ControlSetxxx\Services\<driver>\Parameters

Arguments:

RegistryPath - Registry path passed to DriverEntry

Returns:

NTSTATUS - SUCCESS if able to configure the framework

--*/

{
    PAGED_CODE();

    //
    // Initializing the unicode string, so that if it is not allocated it will not be deallocated too.
    //
    RtlInitUnicodeString(&g_RegistryPath, nullptr);

    g_RegistryPath.MaximumLength = RegistryPath->Length + sizeof(WCHAR);

    g_RegistryPath.Buffer = (PWCH)ExAllocatePool2(POOL_FLAG_PAGED, g_RegistryPath.MaximumLength, DRIVER_TAG);

    if (g_RegistryPath.Buffer == nullptr)
    {
        return STATUS_INSUFFICIENT_RESOURCES;
    }

    RtlAppendUnicodeToString(&g_RegistryPath, RegistryPath->Buffer);

    return STATUS_SUCCESS;
}

PAGED_CODE_SEG
NTSTATUS
Codec_EvtBusDeviceAdd(
    _In_    WDFDRIVER        Driver,
    _Inout_ PWDFDEVICE_INIT  DeviceInit
)
/*++

Routine Description:

    EvtDeviceAdd is called by the framework in response to AddDevice
    call from the PnP manager. We create and initialize a device object to
    represent a new instance of the device. All the software resources
    should be allocated in this callback.

Arguments:
    Driver - Handle to a framework driver object created in DriverEntry

    DeviceInit - Pointer to a framework-allocated WDFDEVICE_INIT structure.

Return Value:

    NTSTATUS

--*/
{
    NTSTATUS                            status = STATUS_SUCCESS;
    WDF_OBJECT_ATTRIBUTES               attributes;
    WDF_DEVICE_PNP_CAPABILITIES         pnpCaps;
    ACX_DEVICEINIT_CONFIG               devInitCfg;
    ACX_DEVICE_CONFIG                   devCfg;
    WDFDEVICE                           device = nullptr;
    PCODEC_DEVICE_CONTEXT               devCtx;
    WDF_PNPPOWER_EVENT_CALLBACKS        pnpPowerCallbacks;

    PAGED_CODE();

    UNREFERENCED_PARAMETER(Driver);

    //
    // The driver calls this DDI in its AddDevice callback before creating the PnP device.
    // ACX uses this call to add default/standard settings for the device to be created.
    //
    ACX_DEVICEINIT_CONFIG_INIT(&devInitCfg);
    RETURN_IF_FAILED(AcxDeviceInitInitialize(DeviceInit, &devInitCfg));

    //
    // Initialize the pnpPowerCallbacks structure.  Callback events for PNP
    // and Power are specified here.  If you don't supply any callbacks,
    // the Framework will take appropriate default actions based on whether
    // DeviceInit is initialized to be an FDO, a PDO or a filter device
    // object.
    //
    WDF_PNPPOWER_EVENT_CALLBACKS_INIT(&pnpPowerCallbacks);
    pnpPowerCallbacks.EvtDevicePrepareHardware = Codec_EvtDevicePrepareHardware;
    pnpPowerCallbacks.EvtDeviceReleaseHardware = Codec_EvtDeviceReleaseHardware;
    pnpPowerCallbacks.EvtDeviceD0Entry = Codec_EvtDeviceD0Entry;
    pnpPowerCallbacks.EvtDeviceD0Exit = Codec_EvtDeviceD0Exit;
    WdfDeviceInitSetPnpPowerEventCallbacks(DeviceInit, &pnpPowerCallbacks);

    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&attributes, CODEC_DEVICE_CONTEXT);
    attributes.EvtCleanupCallback = Codec_EvtDeviceContextCleanup;

    RETURN_NTSTATUS_IF_FAILED(WdfDeviceCreate(&DeviceInit, &attributes, &device));

    //
    // Init Codec's device context.
    //
    devCtx = GetCodecDeviceContext(device);
    ASSERT(devCtx != nullptr);

    devCtx->Render = nullptr;
    devCtx->Capture = nullptr;
    devCtx->ExcludeD3Cold = WdfFalse;

    //
    // The driver calls this DDI in its AddDevice callback after creating the PnP
    // device. ACX uses this call to apply any post device settings.
    //
    ACX_DEVICE_CONFIG_INIT(&devCfg);
    RETURN_NTSTATUS_IF_FAILED(AcxDeviceInitialize(device, &devCfg));

    //
    // Tell the framework to set the SurpriseRemovalOK in the DeviceCaps so
    // that you don't get the popup in usermode (on Win2K) when you surprise
    // remove the device.
    //
    WDF_DEVICE_PNP_CAPABILITIES_INIT(&pnpCaps);
    pnpCaps.SurpriseRemovalOK = WdfTrue;
    WdfDeviceSetPnpCapabilities(device, &pnpCaps);

    //
    // Create a render circuit and capture circuit and add them to the current
    // device context. These circuits will be added to the device when the
    // prepare hardware callback is called. 
    //
    // Spike 3: no static speaker; the render endpoint is a dynamic child (Spike_*).
    RETURN_NTSTATUS_IF_FAILED(Spike_Init(device));

    RETURN_NTSTATUS_IF_FAILED(CodecC_AddStaticCapture(device, &CODEC_CAPTURE_COMPONENT_GUID, &MIC_CUSTOM_NAME, &captureCircuitName));

    return status;
}

PAGED_CODE_SEG
NTSTATUS
Codec_EvtDevicePrepareHardware(
    _In_ WDFDEVICE      Device,
    _In_ WDFCMRESLIST   ResourceList,
    _In_ WDFCMRESLIST   ResourceListTranslated
)
/*++

Routine Description:

    In this callback, the driver does whatever is necessary to make the
    hardware ready to use.

Arguments:

    Device - handle to a device

Return Value:

    NT status value

--*/
{
    NTSTATUS                status = STATUS_SUCCESS;
    PCODEC_DEVICE_CONTEXT   devCtx;

    UNREFERENCED_PARAMETER(ResourceList);
    UNREFERENCED_PARAMETER(ResourceListTranslated);

    PAGED_CODE();

    devCtx = GetCodecDeviceContext(Device);
    ASSERT(devCtx != nullptr);

    // NOTE: Download firmware here.

    // NOTE: Register streaming h/w resources here.

    // Spike 3: no idle power-down (one less variable).

    //
    // Setting up the data saving (CSaveData) and wave file reader (CWaveReader)
    // utility classes which will be used by the virtual streaming engine.
    //
    RETURN_NTSTATUS_IF_FAILED(CSaveData::SetDeviceObject(WdfDeviceWdmGetDeviceObject(Device)));

    RETURN_NTSTATUS_IF_FAILED(CSaveData::InitializeWorkItems(WdfDeviceWdmGetDeviceObject(Device)));

    RETURN_NTSTATUS_IF_FAILED(CWaveReader::InitializeWorkItems(WdfDeviceWdmGetDeviceObject(Device)));

    //
    // The driver uses this DDI to associate a circuit to a device. After
    // this call the circuit is not visible until the device goes in D0.
    // For a real driver there should be a check here to make sure the
    // circuit has not been added already (there could be a situation where
    // prepareHardware is called multiple times and releaseHardware is only
    // called once). 
    //

    ASSERT(devCtx->Capture);
    RETURN_NTSTATUS_IF_FAILED(AcxDeviceAddCircuit(Device, devCtx->Capture));

    Spike_Log(L"parent PrepareHardware: static mic added; rounds start in %lu s", SPIKE_FIRST_DELAY_S);
    WdfTimerStart(g_SpikeTimer, WDF_REL_TIMEOUT_IN_SEC(SPIKE_FIRST_DELAY_S));

    return status;
}

PAGED_CODE_SEG
NTSTATUS
Codec_EvtDeviceReleaseHardware(
    _In_ WDFDEVICE      Device,
    _In_ WDFCMRESLIST   ResourceListTranslated
)
/*++

Routine Description:

    In this callback, the driver releases the h/w resources allocated in the
    prepare h/w callback.

Arguments:

    Device - handle to a device

Return Value:

    NT status value

--*/
{
    NTSTATUS                status;
    PCODEC_DEVICE_CONTEXT   devCtx;

    UNREFERENCED_PARAMETER(Device);
    UNREFERENCED_PARAMETER(ResourceListTranslated);

    PAGED_CODE();

    devCtx = GetCodecDeviceContext(Device);
    ASSERT(devCtx != nullptr);

    //
    // The driver uses this DDI to delete a circuit from the current device. 
    //
    WdfTimerStop(g_SpikeTimer, TRUE);
    Spike_Log(L"parent ReleaseHardware");
    RETURN_NTSTATUS_IF_FAILED(AcxDeviceRemoveCircuit(Device, devCtx->Capture));
    // Spike 3 fix: AcxDeviceRemoveCircuit deletes the circuit; the device cleanup must not
    // touch it afterwards (the sample's Codec_EvtDeviceContextCleanup did: 0x7E under Verifier).
    devCtx->Capture = nullptr;

    // NOTE: Release streaming h/w resources here.

    CSaveData::DestroyWorkItems();
    CWaveReader::DestroyWorkItems();

    status = STATUS_SUCCESS;

    return status;
}

PAGED_CODE_SEG
NTSTATUS
Codec_EvtDeviceD0Entry(
    _In_  WDFDEVICE              Device,
    _In_  WDF_POWER_DEVICE_STATE PreviousState
)
{
    UNREFERENCED_PARAMETER(Device);
    UNREFERENCED_PARAMETER(PreviousState);

    PAGED_CODE();

    return STATUS_SUCCESS;
}

NTSTATUS
Codec_EvtDeviceD0Exit(
    _In_  WDFDEVICE              Device,
    _In_  WDF_POWER_DEVICE_STATE TargetState
)
{
    NTSTATUS        status = STATUS_SUCCESS;
    POWER_ACTION    powerAction;

    PAGED_CODE();

    powerAction = WdfDeviceGetSystemPowerAction(Device);

    // 
    // Update the power policy D3-cold info for Connected Standby.
    //
    if (TargetState == WdfPowerDeviceD3 && powerAction == PowerActionNone)
    {
        PCODEC_DEVICE_CONTEXT   devCtx;
        WDF_TRI_STATE           excludeD3Cold = WdfTrue;
        ACX_DX_EXIT_LATENCY     latency;

        devCtx = GetCodecDeviceContext(Device);
        ASSERT(devCtx != nullptr);

        //
        // Get the current exit latency.
        //
        latency = AcxDeviceGetCurrentDxExitLatency(Device,
            WdfDeviceGetSystemPowerAction(Device),
            TargetState);

        //
        // If the current exit latency for the ACX device is responsive
        // (not instant or fast) then D3-cold does not need to be excluded.
        // Otherwise, D3-cold should be excluded because if the hardware
        // goes into this state it will take too long to go back into D0
        // and respond. 
        //
        if (latency == AcxDxExitLatencyResponsive)
        {
            excludeD3Cold = WdfFalse;
        }

        if (devCtx->ExcludeD3Cold != excludeD3Cold)
        {
            devCtx->ExcludeD3Cold = excludeD3Cold;

            RETURN_NTSTATUS_IF_FAILED(Codec_SetPowerPolicy(Device));
        }
    }

    return status;
}

PAGED_CODE_SEG
NTSTATUS
Codec_SetPowerPolicy(
    _In_ WDFDEVICE      Device
)
{
    NTSTATUS                status = STATUS_SUCCESS;
    PCODEC_DEVICE_CONTEXT   devCtx;

    PAGED_CODE();

    devCtx = GetCodecDeviceContext(Device);
    ASSERT(devCtx != nullptr);

    //
    // Init the idle policy structure.
    //
    WDF_DEVICE_POWER_POLICY_IDLE_SETTINGS idleSettings;
    WDF_DEVICE_POWER_POLICY_IDLE_SETTINGS_INIT(&idleSettings, IdleCannotWakeFromS0);
    idleSettings.IdleTimeout = IDLE_POWER_TIMEOUT;
    idleSettings.IdleTimeoutType = SystemManagedIdleTimeoutWithHint;
    idleSettings.ExcludeD3Cold = devCtx->ExcludeD3Cold;

    RETURN_NTSTATUS_IF_FAILED(WdfDeviceAssignS0IdleSettings(Device, &idleSettings));

    return status;
}

VOID
Codec_EvtDeviceContextCleanup(
    _In_ WDFOBJECT      WdfDevice
)
/*++

Routine Description:

    In this callback, it cleans up device context.

Arguments:

    WdfDevice - WDF device object

Return Value:

    nullptr

--*/
{
    WDFDEVICE               device;
    PCODEC_DEVICE_CONTEXT   devCtx;

    device = (WDFDEVICE)WdfDevice;
    devCtx = GetCodecDeviceContext(device);
    ASSERT(devCtx != nullptr);

    if (devCtx->Capture)
    {
        CodecC_CircuitCleanup(devCtx->Capture);
    }
}


// =====================================================================================
// Spike 3: dynamic endpoints. A timer on the parent creates and destroys a raw-PDO
// child that owns a render circuit, in rounds:
//   r1 instance "9", no interface properties written  (does it appear on its own?)
//   r2 instance "0", interface properties written      (baseline with properties)
//   r3 instance "0" again, new names                   (is the new name picked up?)
//   r4 instance "1", new names                         (fresh instance)
//   r5 instance "2", new names, left alive             (for later tests)
// Each child names itself three ways: device text "SpikeDev rN", bridge pin
// "SpikePin rN", interface FriendlyName "SpikeIf rN" (rounds 2..5).
// Everything is logged as Log### REG_SZ values under the parent's device key.
// =====================================================================================

typedef struct _SPIKE_CHILD_CONTEXT {
    ACXCIRCUIT  Circuit;
    ULONG       Round;
    BOOLEAN     WriteInterfaceProps;
} SPIKE_CHILD_CONTEXT, *PSPIKE_CHILD_CONTEXT;
WDF_DECLARE_CONTEXT_TYPE_WITH_NAME(SPIKE_CHILD_CONTEXT, GetSpikeChildContext)

typedef struct _SPIKE_STEP {
    BOOLEAN Create;          // TRUE: create a child; FALSE: remove the current one
    ULONG   Round;
    PCWSTR  InstanceId;
    BOOLEAN WriteProps;
    ULONG   NextDelayS;      // 0: last step
} SPIKE_STEP;

// v3: r1 lives 60 s while apps play into it, is removed under them, then comes back as r2
// with the same instance ID and stays.
static const SPIKE_STEP g_SpikeSteps[] = {
    { TRUE,  1, L"0", TRUE,  60 }, { FALSE, 1, nullptr, FALSE, 15 },
    { TRUE,  2, L"0", TRUE,   0 },
};

// DEVPKEY_Device_FriendlyName
static const DEVPROPKEY SPIKE_DEVPKEY_Device_FriendlyName = {
    {0xa45c254e, 0xdf1c, 0x4efd, {0x80, 0x20, 0x67, 0xd1, 0x46, 0xa8, 0x50, 0xe0}}, 14 };

ULONG               g_SpikeRound;
static WDFDEVICE    g_SpikeParent;
static WDFDEVICE    g_SpikeChild;
static ULONG        g_SpikeStep;
static LONG         g_SpikeLogCount = -1;

// KS categories the INF registers for the static speaker; written by hand for children.
static const GUID SPIKE_KSCATEGORY_AUDIO    = {0x6994AD04, 0x93EF, 0x11D0, {0xA3, 0xCC, 0x00, 0xA0, 0xC9, 0x22, 0x31, 0x96}};
static const GUID SPIKE_KSCATEGORY_RENDER   = {0x65E8773E, 0x8F56, 0x11D0, {0xA3, 0xB9, 0x00, 0xA0, 0xC9, 0x22, 0x31, 0x96}};
static const GUID SPIKE_KSCATEGORY_REALTIME = {0xEB115FFC, 0x10C8, 0x4964, {0x83, 0x1D, 0x6D, 0xCB, 0x02, 0xE6, 0xF2, 0x3F}};
static const GUID SPIKE_GUID_DEVCLASS_MEDIA = {0x4d36e96c, 0xe325, 0x11ce, {0xbf, 0xc1, 0x08, 0x00, 0x2b, 0xe1, 0x03, 0x18}};

VOID
Spike_Log(
    _In_z_ _Printf_format_string_ PCWSTR Format,
    ...
)
{
    WCHAR           text[600];
    WCHAR           valueName[16];
    va_list         args;
    WDFKEY          key;
    size_t          len;
    UNICODE_STRING  name;

    if (KeGetCurrentIrql() != PASSIVE_LEVEL || g_SpikeParent == nullptr) {
        return;
    }
    va_start(args, Format);
    (VOID)RtlStringCbVPrintfW(text, sizeof(text), Format, args);
    va_end(args);
    DbgPrintEx(DPFLTR_IHVDRIVER_ID, DPFLTR_ERROR_LEVEL, "spike3: %ws\n", text);

    if (!NT_SUCCESS(WdfDeviceOpenRegistryKey(g_SpikeParent, PLUGPLAY_REGKEY_DEVICE, KEY_SET_VALUE,
                                             WDF_NO_OBJECT_ATTRIBUTES, &key))) {
        return;
    }
    (VOID)RtlStringCbPrintfW(valueName, sizeof(valueName), L"Log%03ld", InterlockedIncrement(&g_SpikeLogCount));
    RtlInitUnicodeString(&name, valueName);
    if (NT_SUCCESS(RtlStringCbLengthW(text, sizeof(text), &len))) {
        (VOID)WdfRegistryAssignValue(key, &name, REG_SZ, (ULONG)(len + sizeof(WCHAR)), text);
    }
    WdfRegistryClose(key);
}

static NTSTATUS
Spike_SetSz(_In_ HANDLE Key, _In_z_ PCWSTR Name, _In_z_ PCWSTR Value)
{
    UNICODE_STRING  n;
    size_t          len;

    RtlInitUnicodeString(&n, Name);
    RETURN_NTSTATUS_IF_FAILED(RtlStringCbLengthW(Value, 512, &len));
    return ZwSetValueKey(Key, &n, 0, REG_SZ, (PVOID)Value, (ULONG)(len + sizeof(WCHAR)));
}

static NTSTATUS
Spike_CreateSubkey(_In_ HANDLE Parent, _In_z_ PCWSTR Name, _Out_ HANDLE* Key)
{
    UNICODE_STRING      n;
    OBJECT_ATTRIBUTES   oa;

    RtlInitUnicodeString(&n, Name);
    InitializeObjectAttributes(&oa, &n, OBJ_KERNEL_HANDLE | OBJ_CASE_INSENSITIVE, Parent, nullptr);
    return ZwCreateKey(Key, KEY_ALL_ACCESS, &oa, 0, nullptr, REG_OPTION_NON_VOLATILE, nullptr);
}

// What the INF's AddInterface sections give the static speaker, written for a child.
PAGED_CODE_SEG
static VOID
Spike_WriteInterfaceProps(_In_ WDFDEVICE Child, _In_ PSPIKE_CHILD_CONTEXT Ctx)
{
    static const GUID* categories[] = { &SPIKE_KSCATEGORY_AUDIO, &SPIKE_KSCATEGORY_RENDER, &SPIKE_KSCATEGORY_REALTIME };
    DECLARE_CONST_UNICODE_STRING(refString, L"Speaker0");
    WCHAR           friendly[64];
    PDEVICE_OBJECT  pdo = WdfDeviceWdmGetDeviceObject(Child);

    PAGED_CODE();
    (VOID)RtlStringCbPrintfW(friendly, sizeof(friendly), L"SpikeIf r%lu", Ctx->Round);

    for (ULONG i = 0; i < ARRAYSIZE(categories); i++) {
        UNICODE_STRING  link = {};
        HANDLE          key = nullptr, ep = nullptr, ep0 = nullptr;
        ULONG           one = 1;
        NTSTATUS        st = IoRegisterDeviceInterface(pdo, categories[i], (PUNICODE_STRING)&refString, &link);

        Spike_Log(L"r%lu IoRegisterDeviceInterface cat %lu: 0x%08X %.*ws", Ctx->Round, i, st, (int)(link.Length / sizeof(WCHAR)), link.Buffer ? link.Buffer : L"");
        if (!NT_SUCCESS(st)) {
            continue;
        }
        st = IoOpenDeviceInterfaceRegistryKey(&link, KEY_ALL_ACCESS, &key);
        if (NT_SUCCESS(st)) {
            NTSTATUS a = Spike_SetSz(key, L"CLSID", L"{17CCA71B-ECD7-11D0-B908-00A0C9223196}");
            NTSTATUS b = Spike_SetSz(key, L"FriendlyName", friendly);
            NTSTATUS c = Spike_CreateSubkey(key, L"EP", &ep);
            if (NT_SUCCESS(c)) {
                c = Spike_CreateSubkey(ep, L"0", &ep0);
            }
            if (NT_SUCCESS(c)) {
                UNICODE_STRING n;
                (VOID)Spike_SetSz(ep0, L"{1DA5D803-D492-4EDD-8C23-E0C0FFEE7F0E},2", L"{00000000-0000-0000-0000-000000000000}");
                RtlInitUnicodeString(&n, L"{1DA5D803-D492-4EDD-8C23-E0C0FFEE7F0E},7");
                (VOID)ZwSetValueKey(ep0, &n, 0, REG_DWORD, &one, sizeof(one));
            }
            Spike_Log(L"r%lu interface props cat %lu: CLSID 0x%08X FriendlyName 0x%08X EP\\0 0x%08X", Ctx->Round, i, a, b, c);
            if (ep0) ZwClose(ep0);
            if (ep) ZwClose(ep);
            ZwClose(key);
        } else {
            Spike_Log(L"r%lu IoOpenDeviceInterfaceRegistryKey cat %lu: 0x%08X", Ctx->Round, i, st);
        }
        RtlFreeUnicodeString(&link);
    }
}

PAGED_CODE_SEG
static NTSTATUS
Spike_EvtChildPrepareHardware(_In_ WDFDEVICE Device, _In_ WDFCMRESLIST Resources, _In_ WDFCMRESLIST Translated)
{
    UNREFERENCED_PARAMETER(Resources);
    UNREFERENCED_PARAMETER(Translated);
    PAGED_CODE();

    PSPIKE_CHILD_CONTEXT ctx = GetSpikeChildContext(Device);
    WCHAR friendly[64];
    size_t len = 0;
    (VOID)RtlStringCbPrintfW(friendly, sizeof(friendly), L"Spike r%lu", ctx->Round);
    (VOID)RtlStringCbLengthW(friendly, sizeof(friendly), &len);
    NTSTATUS nameSt = IoSetDevicePropertyData(WdfDeviceWdmGetDeviceObject(Device), &SPIKE_DEVPKEY_Device_FriendlyName,
                                              LOCALE_NEUTRAL, PLUGPLAY_PROPERTY_PERSISTENT, DEVPROP_TYPE_STRING,
                                              (ULONG)(len + sizeof(WCHAR)), friendly);

    // v2: the circuit is created here, each time: PnP can stop and restart a child
    // (first install, rebalance), and ReleaseHardware's AcxDeviceRemoveCircuit deletes it.
    NTSTATUS st = CodecR_CreateRenderCircuit(Device, &CODEC_RENDER_COMPONENT_GUID, &renderCircuitName, &ctx->Circuit);
    if (NT_SUCCESS(st)) {
        st = AcxDeviceAddCircuit(Device, ctx->Circuit);
    }
    Spike_Log(L"r%lu child PrepareHardware: FriendlyName '%ws' 0x%08X, circuit create+add 0x%08X", ctx->Round, friendly, nameSt, st);
    if (NT_SUCCESS(st) && ctx->WriteInterfaceProps) {
        Spike_WriteInterfaceProps(Device, ctx);
    }
    return st;
}

PAGED_CODE_SEG
static NTSTATUS
Spike_EvtChildReleaseHardware(_In_ WDFDEVICE Device, _In_ WDFCMRESLIST Translated)
{
    UNREFERENCED_PARAMETER(Translated);
    PAGED_CODE();

    PSPIKE_CHILD_CONTEXT ctx = GetSpikeChildContext(Device);
    NTSTATUS st = ctx->Circuit ? AcxDeviceRemoveCircuit(Device, ctx->Circuit) : STATUS_SUCCESS;
    ctx->Circuit = nullptr;
    Spike_Log(L"r%lu child ReleaseHardware: AcxDeviceRemoveCircuit 0x%08X", ctx->Round, st);
    return STATUS_SUCCESS;
}

PAGED_CODE_SEG
static NTSTATUS
Spike_EvtChildD0Entry(_In_ WDFDEVICE Device, _In_ WDF_POWER_DEVICE_STATE Previous)
{
    PAGED_CODE();
    PSPIKE_CHILD_CONTEXT ctx = GetSpikeChildContext(Device);
    WDFSTRING linkString = ctx->Circuit ? AcxCircuitGetSymbolicLinkName(ctx->Circuit) : nullptr;
    UNICODE_STRING link = {};
    if (linkString != nullptr) {
        WdfStringGetUnicodeString(linkString, &link);
    }
    Spike_Log(L"r%lu child D0Entry from state %d; circuit link '%.*ws'", ctx->Round, (int)Previous,
              (int)(link.Length / sizeof(WCHAR)), link.Buffer ? link.Buffer : L"");
    return STATUS_SUCCESS;
}

static VOID
Spike_EvtChildCleanup(_In_ WDFOBJECT Object)
{
    Spike_Log(L"r%lu child cleaned up", GetSpikeChildContext((WDFDEVICE)Object)->Round);
}

PAGED_CODE_SEG
static NTSTATUS
Spike_CreateChild(_In_ const SPIKE_STEP* Step)
{
    NTSTATUS                        st;
    PWDFDEVICE_INIT                 init;
    WDFDEVICE                       child = nullptr;
    WDF_OBJECT_ATTRIBUTES           attributes;
    WDF_PNPPOWER_EVENT_CALLBACKS    pnp;
    ACX_DEVICEINIT_CONFIG           acxInit;
    ACX_DEVICE_CONFIG               acxDev;
    PRENDER_DEVICE_CONTEXT          renderCtx;
    PSPIKE_CHILD_CONTEXT            ctx;
    WCHAR                           textBuf[64];
    UNICODE_STRING                  text, instance;
    DECLARE_CONST_UNICODE_STRING(deviceId, L"SoundboardSpike\\Endpoint");
    DECLARE_CONST_UNICODE_STRING(location, L"Soundboard spike");
    // SYSTEM all, admins RWX, everyone RW, restricted R: the usual for a media device.
    DECLARE_CONST_UNICODE_STRING(sddl, L"D:P(A;;GA;;;SY)(A;;GRGWGX;;;BA)(A;;GRGW;;;WD)(A;;GR;;;RC)");

    PAGED_CODE();
    g_SpikeRound = Step->Round;

    init = WdfPdoInitAllocate(g_SpikeParent);
    if (init == nullptr) {
        Spike_Log(L"r%lu WdfPdoInitAllocate failed", Step->Round);
        return STATUS_INSUFFICIENT_RESOURCES;
    }
    (VOID)RtlStringCbPrintfW(textBuf, sizeof(textBuf), L"SpikeDev r%lu", Step->Round);
    RtlInitUnicodeString(&text, textBuf);
    RtlInitUnicodeString(&instance, Step->InstanceId);

#define SPIKE_STEP_OR_FREE(call) do { st = (call); if (!NT_SUCCESS(st)) { \
        Spike_Log(L"r%lu %S: 0x%08X", Step->Round, #call, st); WdfDeviceInitFree(init); return st; } } while (0)

    SPIKE_STEP_OR_FREE(WdfPdoInitAssignRawDevice(init, &SPIKE_GUID_DEVCLASS_MEDIA));
    SPIKE_STEP_OR_FREE(WdfDeviceInitAssignSDDLString(init, &sddl));
    SPIKE_STEP_OR_FREE(WdfPdoInitAssignDeviceID(init, &deviceId));
    SPIKE_STEP_OR_FREE(WdfPdoInitAddHardwareID(init, &deviceId));
    SPIKE_STEP_OR_FREE(WdfPdoInitAssignInstanceID(init, &instance));
    SPIKE_STEP_OR_FREE(WdfPdoInitAddDeviceText(init, &text, &location, 0x409));
    WdfPdoInitSetDefaultLocale(init, 0x409);

    ACX_DEVICEINIT_CONFIG_INIT(&acxInit);
    acxInit.Flags |= AcxDeviceInitConfigRawDevice;
    SPIKE_STEP_OR_FREE(AcxDeviceInitInitialize(init, &acxInit));

    WDF_PNPPOWER_EVENT_CALLBACKS_INIT(&pnp);
    pnp.EvtDevicePrepareHardware = Spike_EvtChildPrepareHardware;
    pnp.EvtDeviceReleaseHardware = Spike_EvtChildReleaseHardware;
    pnp.EvtDeviceD0Entry = Spike_EvtChildD0Entry;
    WdfDeviceInitSetPnpPowerEventCallbacks(init, &pnp);

    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&attributes, SPIKE_CHILD_CONTEXT);
    attributes.EvtCleanupCallback = Spike_EvtChildCleanup;
    SPIKE_STEP_OR_FREE(WdfDeviceCreate(&init, &attributes, &child));
#undef SPIKE_STEP_OR_FREE

#define SPIKE_STEP_OR_DELETE(call) do { st = (call); if (!NT_SUCCESS(st)) { \
        Spike_Log(L"r%lu %S: 0x%08X", Step->Round, #call, st); WdfObjectDelete(child); return st; } } while (0)

    ctx = GetSpikeChildContext(child);
    ctx->Round = Step->Round;
    ctx->WriteInterfaceProps = Step->WriteProps;

    // The sample's render stream code reads a RENDER_DEVICE_CONTEXT from the circuit's device.
    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&attributes, RENDER_DEVICE_CONTEXT);
    SPIKE_STEP_OR_DELETE(WdfObjectAllocateContext(child, &attributes, (PVOID*)&renderCtx));

    ACX_DEVICE_CONFIG_INIT(&acxDev);
    SPIKE_STEP_OR_DELETE(AcxDeviceInitialize(child, &acxDev));
    SPIKE_STEP_OR_DELETE(AcxDeviceAddCircuitDevice(g_SpikeParent, child));
#undef SPIKE_STEP_OR_DELETE

    g_SpikeChild = child;
    Spike_Log(L"r%lu child created: instance '%ws', props %d", Step->Round, Step->InstanceId, (int)Step->WriteProps);
    return STATUS_SUCCESS;
}

static VOID
Spike_EvtTimer(_In_ WDFTIMER Timer)
{
    UNREFERENCED_PARAMETER(Timer);

    if (g_SpikeStep >= ARRAYSIZE(g_SpikeSteps)) {
        return;
    }
    const SPIKE_STEP* step = &g_SpikeSteps[g_SpikeStep++];
    if (step->Create) {
        (VOID)Spike_CreateChild(step);
    } else if (g_SpikeChild != nullptr) {
        NTSTATUS st = AcxDeviceRemoveCircuitDevice(g_SpikeParent, g_SpikeChild);
        Spike_Log(L"r%lu AcxDeviceRemoveCircuitDevice: 0x%08X", step->Round, st);
        g_SpikeChild = nullptr;
    }
    if (step->NextDelayS != 0) {
        WdfTimerStart(g_SpikeTimer, WDF_REL_TIMEOUT_IN_SEC(step->NextDelayS));
    } else {
        Spike_Log(L"rounds done");
    }
}

PAGED_CODE_SEG
NTSTATUS
Spike_Init(_In_ WDFDEVICE Parent)
{
    WDF_TIMER_CONFIG        config;
    WDF_OBJECT_ATTRIBUTES   attributes;

    PAGED_CODE();
    g_SpikeParent = Parent;
    WDF_TIMER_CONFIG_INIT(&config, Spike_EvtTimer);
    config.AutomaticSerialization = FALSE;
    WDF_OBJECT_ATTRIBUTES_INIT(&attributes);
    attributes.ParentObject = Parent;
    attributes.ExecutionLevel = WdfExecutionLevelPassive;
    RETURN_NTSTATUS_IF_FAILED(WdfTimerCreate(&config, &attributes, &g_SpikeTimer));
    Spike_Log(L"spike3 loaded");
    return STATUS_SUCCESS;
}
