/******************************Module*Header*******************************\
* Module Name: BDD_DDI.cxx
*
* Basic Display Driver DDI entry points redirects
*
*
* Copyright (c) 2010 Microsoft Corporation
\**************************************************************************/

#include "BDD.hxx"


#pragma code_seg(push)
#pragma code_seg("INIT")
// BEGIN: Init Code

//
// Driver Entry point
//

extern "C"
NTSTATUS
DriverEntry(
    _In_  DRIVER_OBJECT*  pDriverObject,
    _In_  UNICODE_STRING* pRegistryPath)
{
    PAGED_CODE();

    BDD_PROBE3("DriverEntry driverObject=0x%I64x registryPath=0x%I64x registryLength=%u",
               (ULONGLONG)(ULONG_PTR)pDriverObject,
               (ULONGLONG)(ULONG_PTR)pRegistryPath,
               (pRegistryPath != NULL) ? pRegistryPath->Length : 0);

    // Initialize DDI function pointers and dxgkrnl
    KMDDOD_INITIALIZATION_DATA InitialData = {0};

    InitialData.Version = DXGKDDI_INTERFACE_VERSION;

    InitialData.DxgkDdiAddDevice                    = BddDdiAddDevice;
    InitialData.DxgkDdiStartDevice                  = BddDdiStartDevice;
    InitialData.DxgkDdiStopDevice                   = BddDdiStopDevice;
    InitialData.DxgkDdiResetDevice                  = BddDdiResetDevice;
    InitialData.DxgkDdiRemoveDevice                 = BddDdiRemoveDevice;
    InitialData.DxgkDdiDispatchIoRequest            = BddDdiDispatchIoRequest;
    InitialData.DxgkDdiInterruptRoutine             = BddDdiInterruptRoutine;
    InitialData.DxgkDdiDpcRoutine                   = BddDdiDpcRoutine;
    InitialData.DxgkDdiQueryChildRelations          = BddDdiQueryChildRelations;
    InitialData.DxgkDdiQueryChildStatus             = BddDdiQueryChildStatus;
    InitialData.DxgkDdiQueryDeviceDescriptor        = BddDdiQueryDeviceDescriptor;
    InitialData.DxgkDdiSetPowerState                = BddDdiSetPowerState;
    InitialData.DxgkDdiUnload                       = BddDdiUnload;
    InitialData.DxgkDdiQueryAdapterInfo             = BddDdiQueryAdapterInfo;
    InitialData.DxgkDdiSetPointerPosition           = BddDdiSetPointerPosition;
    InitialData.DxgkDdiSetPointerShape              = BddDdiSetPointerShape;
    InitialData.DxgkDdiIsSupportedVidPn             = BddDdiIsSupportedVidPn;
    InitialData.DxgkDdiRecommendFunctionalVidPn     = BddDdiRecommendFunctionalVidPn;
    InitialData.DxgkDdiEnumVidPnCofuncModality      = BddDdiEnumVidPnCofuncModality;
    InitialData.DxgkDdiSetVidPnSourceVisibility     = BddDdiSetVidPnSourceVisibility;
    InitialData.DxgkDdiCommitVidPn                  = BddDdiCommitVidPn;
    InitialData.DxgkDdiUpdateActiveVidPnPresentPath = BddDdiUpdateActiveVidPnPresentPath;
    InitialData.DxgkDdiRecommendMonitorModes        = BddDdiRecommendMonitorModes;
    InitialData.DxgkDdiQueryVidPnHWCapability       = BddDdiQueryVidPnHWCapability;
    InitialData.DxgkDdiPresentDisplayOnly           = BddDdiPresentDisplayOnly;
    InitialData.DxgkDdiStopDeviceAndReleasePostDisplayOwnership = BddDdiStopDeviceAndReleasePostDisplayOwnership;
    InitialData.DxgkDdiSystemDisplayEnable          = BddDdiSystemDisplayEnable;
    InitialData.DxgkDdiSystemDisplayWrite           = BddDdiSystemDisplayWrite;

    BDD_PROBE0("DriverEntry registering display-only DDIs; SetVidPnSourceAddress/MPO callbacks are not registered by KMDOD");

    NTSTATUS Status = DxgkInitializeDisplayOnlyDriver(pDriverObject, pRegistryPath, &InitialData);
    if (!NT_SUCCESS(Status))
    {
        BDD_LOG_ERROR1("DxgkInitializeDisplayOnlyDriver failed with Status: 0x%I64x", Status);
        BDD_PROBE1("DriverEntry DxgkInitializeDisplayOnlyDriver failed status=0x%08x", Status);
        return Status;
    }

    BDD_PROBE1("DriverEntry success status=0x%08x", Status);

    return Status;
}
// END: Init Code
#pragma code_seg(pop)

#pragma code_seg(push)
#pragma code_seg("PAGE")

//
// PnP DDIs
//

VOID
BddDdiUnload(VOID)
{
    PAGED_CODE();
}

NTSTATUS
BddDdiAddDevice(
    _In_ DEVICE_OBJECT* pPhysicalDeviceObject,
    _Outptr_ PVOID*  ppDeviceContext)
{
    PAGED_CODE();

    BDD_PROBE2("AddDevice pdo=0x%I64x ppDeviceContext=0x%I64x",
               (ULONGLONG)(ULONG_PTR)pPhysicalDeviceObject,
               (ULONGLONG)(ULONG_PTR)ppDeviceContext);

    if ((pPhysicalDeviceObject == NULL) ||
        (ppDeviceContext == NULL))
    {
        BDD_LOG_ERROR2("One of pPhysicalDeviceObject (0x%I64x), ppDeviceContext (0x%I64x) is NULL",
                        pPhysicalDeviceObject, ppDeviceContext);
        return STATUS_INVALID_PARAMETER;
    }
    *ppDeviceContext = NULL;

    BASIC_DISPLAY_DRIVER* pBDD = new(NonPagedPoolNx) BASIC_DISPLAY_DRIVER(pPhysicalDeviceObject);
    if (pBDD == NULL)
    {
        BDD_LOG_LOW_RESOURCE0("pBDD failed to be allocated");
        return STATUS_NO_MEMORY;
    }

    *ppDeviceContext = pBDD;

    BDD_PROBE2("AddDevice created context=0x%I64x for pdo=0x%I64x",
               (ULONGLONG)(ULONG_PTR)pBDD,
               (ULONGLONG)(ULONG_PTR)pPhysicalDeviceObject);

    return STATUS_SUCCESS;
}

NTSTATUS
BddDdiRemoveDevice(
    _In_  VOID* pDeviceContext)
{
    PAGED_CODE();

    BASIC_DISPLAY_DRIVER* pBDD = reinterpret_cast<BASIC_DISPLAY_DRIVER*>(pDeviceContext);

    if (pBDD)
    {
        delete pBDD;
        pBDD = NULL;
    }

    return STATUS_SUCCESS;
}

NTSTATUS
BddDdiStartDevice(
    _In_  VOID*              pDeviceContext,
    _In_  DXGK_START_INFO*   pDxgkStartInfo,
    _In_  DXGKRNL_INTERFACE* pDxgkInterface,
    _Out_ ULONG*             pNumberOfViews,
    _Out_ ULONG*             pNumberOfChildren)
{
    PAGED_CODE();
    BDD_ASSERT_CHK(pDeviceContext != NULL);

    BASIC_DISPLAY_DRIVER* pBDD = reinterpret_cast<BASIC_DISPLAY_DRIVER*>(pDeviceContext);
    BDD_PROBE4("StartDevice ddi context=0x%I64x startInfo=0x%I64x dxgkInterface=0x%I64x deviceHandle=0x%I64x",
               (ULONGLONG)(ULONG_PTR)pBDD,
               (ULONGLONG)(ULONG_PTR)pDxgkStartInfo,
               (ULONGLONG)(ULONG_PTR)pDxgkInterface,
               (pDxgkInterface != NULL) ? (ULONGLONG)(ULONG_PTR)pDxgkInterface->DeviceHandle : 0);

    NTSTATUS Status = pBDD->StartDevice(pDxgkStartInfo, pDxgkInterface, pNumberOfViews, pNumberOfChildren);
    BDD_PROBE3("StartDevice ddi result status=0x%08x views=%u children=%u",
               Status,
               (pNumberOfViews != NULL) ? *pNumberOfViews : 0,
               (pNumberOfChildren != NULL) ? *pNumberOfChildren : 0);
    return Status;
}

NTSTATUS
BddDdiStopDevice(
    _In_  VOID* pDeviceContext)
{
    PAGED_CODE();
    BDD_ASSERT_CHK(pDeviceContext != NULL);

    BASIC_DISPLAY_DRIVER* pBDD = reinterpret_cast<BASIC_DISPLAY_DRIVER*>(pDeviceContext);
    BDD_PROBE1("StopDevice ddi context=0x%I64x", (ULONGLONG)(ULONG_PTR)pBDD);
    NTSTATUS Status = pBDD->StopDevice();
    BDD_PROBE1("StopDevice ddi result status=0x%08x", Status);
    return Status;
}


NTSTATUS
BddDdiDispatchIoRequest(
    _In_  VOID*                 pDeviceContext,
    _In_  ULONG                 VidPnSourceId,
    _In_  VIDEO_REQUEST_PACKET* pVideoRequestPacket)
{
    PAGED_CODE();
    BDD_ASSERT_CHK(pDeviceContext != NULL);

    BASIC_DISPLAY_DRIVER* pBDD = reinterpret_cast<BASIC_DISPLAY_DRIVER*>(pDeviceContext);
    if (!pBDD->IsDriverActive())
    {
        BDD_LOG_ASSERTION1("BDD (0x%I64x) is being called when not active!", pBDD);
        return STATUS_UNSUCCESSFUL;
    }
    return pBDD->DispatchIoRequest(VidPnSourceId, pVideoRequestPacket);
}

NTSTATUS
BddDdiSetPowerState(
    _In_  VOID*              pDeviceContext,
    _In_  ULONG              HardwareUid,
    _In_  DEVICE_POWER_STATE DevicePowerState,
    _In_  POWER_ACTION       ActionType)
{
    PAGED_CODE();
    BDD_ASSERT_CHK(pDeviceContext != NULL);

    BASIC_DISPLAY_DRIVER* pBDD = reinterpret_cast<BASIC_DISPLAY_DRIVER*>(pDeviceContext);
    if (!pBDD->IsDriverActive())
    {
        // If the driver isn't active, SetPowerState can still be called, however in BDD's case
        // this shouldn't do anything, as it could for instance be called on BDD Fallback after
        // Fallback has been stopped and BDD PnP is being started. Fallback doesn't have control
        // of the hardware in this case.
        return STATUS_SUCCESS;
    }
    return pBDD->SetPowerState(HardwareUid, DevicePowerState, ActionType);
}

NTSTATUS
BddDdiQueryChildRelations(
    _In_                             VOID*                  pDeviceContext,
    _Out_writes_bytes_(ChildRelationsSize) DXGK_CHILD_DESCRIPTOR* pChildRelations,
    _In_                             ULONG                  ChildRelationsSize)
{
    PAGED_CODE();
    BDD_ASSERT_CHK(pDeviceContext != NULL);

    BASIC_DISPLAY_DRIVER* pBDD = reinterpret_cast<BASIC_DISPLAY_DRIVER*>(pDeviceContext);
    return pBDD->QueryChildRelations(pChildRelations, ChildRelationsSize);
}

NTSTATUS
BddDdiQueryChildStatus(
    _In_    VOID*              pDeviceContext,
    _Inout_ DXGK_CHILD_STATUS* pChildStatus,
    _In_    BOOLEAN            NonDestructiveOnly)
{
    PAGED_CODE();
    BDD_ASSERT_CHK(pDeviceContext != NULL);

    BASIC_DISPLAY_DRIVER* pBDD = reinterpret_cast<BASIC_DISPLAY_DRIVER*>(pDeviceContext);
    return pBDD->QueryChildStatus(pChildStatus, NonDestructiveOnly);
}

NTSTATUS
BddDdiQueryDeviceDescriptor(
    _In_  VOID*                     pDeviceContext,
    _In_  ULONG                     ChildUid,
    _Inout_ DXGK_DEVICE_DESCRIPTOR* pDeviceDescriptor)
{
    PAGED_CODE();
    BDD_ASSERT_CHK(pDeviceContext != NULL);

    BASIC_DISPLAY_DRIVER* pBDD = reinterpret_cast<BASIC_DISPLAY_DRIVER*>(pDeviceContext);
    if (!pBDD->IsDriverActive())
    {
        // During stress testing of PnPStop, it is possible for BDD Fallback to get called to start then stop in quick succession.
        // The first call queues a worker thread item indicating that it now has a child device, the second queues a worker thread
        // item that it no longer has any child device. This function gets called based on the first worker thread item, but after
        // the driver has been stopped. Therefore instead of asserting like other functions, we only warn.
        BDD_LOG_WARNING1("BDD (0x%I64x) is being called when not active!", pBDD);
        return STATUS_UNSUCCESSFUL;
    }
    return pBDD->QueryDeviceDescriptor(ChildUid, pDeviceDescriptor);
}


//
// WDDM Display Only Driver DDIs
//

NTSTATUS
APIENTRY
BddDdiQueryAdapterInfo(
    _In_ CONST HANDLE                    hAdapter,
    _In_ CONST DXGKARG_QUERYADAPTERINFO* pQueryAdapterInfo)
{
    PAGED_CODE();
    BDD_ASSERT_CHK(hAdapter != NULL);

    BASIC_DISPLAY_DRIVER* pBDD = reinterpret_cast<BASIC_DISPLAY_DRIVER*>(hAdapter);
    BDD_PROBE4("QueryAdapterInfo ddi adapter=0x%I64x type=%u inputBytes=%u outputBytes=%u",
               (ULONGLONG)(ULONG_PTR)hAdapter,
               (pQueryAdapterInfo != NULL) ? pQueryAdapterInfo->Type : 0,
               (pQueryAdapterInfo != NULL) ? pQueryAdapterInfo->InputDataSize : 0,
               (pQueryAdapterInfo != NULL) ? pQueryAdapterInfo->OutputDataSize : 0);
    NTSTATUS Status = pBDD->QueryAdapterInfo(pQueryAdapterInfo);
    BDD_PROBE1("QueryAdapterInfo ddi result status=0x%08x", Status);
    return Status;
}

NTSTATUS
APIENTRY
BddDdiSetPointerPosition(
    _In_ CONST HANDLE                      hAdapter,
    _In_ CONST DXGKARG_SETPOINTERPOSITION* pSetPointerPosition)
{
    PAGED_CODE();
    BDD_ASSERT_CHK(hAdapter != NULL);

    BASIC_DISPLAY_DRIVER* pBDD = reinterpret_cast<BASIC_DISPLAY_DRIVER*>(hAdapter);
    if (!pBDD->IsDriverActive())
    {
        BDD_LOG_ASSERTION1("BDD (0x%I64x) is being called when not active!", pBDD);
        return STATUS_UNSUCCESSFUL;
    }
    return pBDD->SetPointerPosition(pSetPointerPosition);
}

NTSTATUS
APIENTRY
BddDdiSetPointerShape(
    _In_ CONST HANDLE                   hAdapter,
    _In_ CONST DXGKARG_SETPOINTERSHAPE* pSetPointerShape)
{
    PAGED_CODE();
    BDD_ASSERT_CHK(hAdapter != NULL);

    BASIC_DISPLAY_DRIVER* pBDD = reinterpret_cast<BASIC_DISPLAY_DRIVER*>(hAdapter);
    if (!pBDD->IsDriverActive())
    {
        BDD_LOG_ASSERTION1("BDD (0x%I64x) is being called when not active!", pBDD);
        return STATUS_UNSUCCESSFUL;
    }
    return pBDD->SetPointerShape(pSetPointerShape);
}


NTSTATUS
APIENTRY
BddDdiPresentDisplayOnly(
    _In_ CONST HANDLE                       hAdapter,
    _In_ CONST DXGKARG_PRESENT_DISPLAYONLY* pPresentDisplayOnly)
{
    PAGED_CODE();
    BDD_ASSERT_CHK(hAdapter != NULL);

    BASIC_DISPLAY_DRIVER* pBDD = reinterpret_cast<BASIC_DISPLAY_DRIVER*>(hAdapter);
    if (!pBDD->IsDriverActive())
    {
        BDD_LOG_ASSERTION1("BDD (0x%I64x) is being called when not active!", pBDD);
        return STATUS_UNSUCCESSFUL;
    }
    BDD_PROBE4("PresentDisplayOnly ddi adapter=0x%I64x sourceId=%u source=0x%I64x pitch=%u",
               (ULONGLONG)(ULONG_PTR)hAdapter,
               (pPresentDisplayOnly != NULL) ? pPresentDisplayOnly->VidPnSourceId : 0,
               (pPresentDisplayOnly != NULL) ? (ULONGLONG)(ULONG_PTR)pPresentDisplayOnly->pSource : 0,
               (pPresentDisplayOnly != NULL) ? pPresentDisplayOnly->Pitch : 0);
    NTSTATUS Status = pBDD->PresentDisplayOnly(pPresentDisplayOnly);
    BDD_PROBE1("PresentDisplayOnly ddi result status=0x%08x", Status);
    return Status;
}

NTSTATUS
APIENTRY
BddDdiStopDeviceAndReleasePostDisplayOwnership(
    _In_  VOID*                          pDeviceContext,
    _In_  D3DDDI_VIDEO_PRESENT_TARGET_ID TargetId,
    _Out_ DXGK_DISPLAY_INFORMATION*      DisplayInfo)
{
    PAGED_CODE();
    BDD_ASSERT_CHK(pDeviceContext != NULL);

    BASIC_DISPLAY_DRIVER* pBDD = reinterpret_cast<BASIC_DISPLAY_DRIVER*>(pDeviceContext);
    return pBDD->StopDeviceAndReleasePostDisplayOwnership(TargetId, DisplayInfo);
}

NTSTATUS
APIENTRY
BddDdiIsSupportedVidPn(
    _In_ CONST HANDLE                 hAdapter,
    _Inout_ DXGKARG_ISSUPPORTEDVIDPN* pIsSupportedVidPn)
{
    PAGED_CODE();
    BDD_ASSERT_CHK(hAdapter != NULL);

    BASIC_DISPLAY_DRIVER* pBDD = reinterpret_cast<BASIC_DISPLAY_DRIVER*>(hAdapter);
    if (!pBDD->IsDriverActive())
    {
        // This path might hit because win32k/dxgport doesn't check that an adapter is active when taking the adapter lock.
        // The adapter lock is the main thing BDD Fallback relies on to not be called while it's inactive. It is still a rare
        // timing issue around PnpStart/Stop and isn't expected to have any effect on the stability of the system.
        BDD_LOG_WARNING1("BDD (0x%I64x) is being called when not active!", pBDD);
        return STATUS_UNSUCCESSFUL;
    }
    BDD_PROBE2("IsSupportedVidPn ddi adapter=0x%I64x desiredVidPn=0x%I64x",
               (ULONGLONG)(ULONG_PTR)hAdapter,
               (pIsSupportedVidPn != NULL) ? (ULONGLONG)(ULONG_PTR)pIsSupportedVidPn->hDesiredVidPn : 0);
    NTSTATUS Status = pBDD->IsSupportedVidPn(pIsSupportedVidPn);
    BDD_PROBE2("IsSupportedVidPn ddi result status=0x%08x supported=%u",
               Status,
               (pIsSupportedVidPn != NULL) ? pIsSupportedVidPn->IsVidPnSupported : 0);
    return Status;
}

NTSTATUS
APIENTRY
BddDdiRecommendFunctionalVidPn(
    _In_ CONST HANDLE                                  hAdapter,
    _In_ CONST DXGKARG_RECOMMENDFUNCTIONALVIDPN* CONST pRecommendFunctionalVidPn)
{
    PAGED_CODE();
    BDD_ASSERT_CHK(hAdapter != NULL);

    BASIC_DISPLAY_DRIVER* pBDD = reinterpret_cast<BASIC_DISPLAY_DRIVER*>(hAdapter);
    if (!pBDD->IsDriverActive())
    {
        BDD_LOG_ASSERTION1("BDD (0x%I64x) is being called when not active!", pBDD);
        return STATUS_UNSUCCESSFUL;
    }
    BDD_PROBE2("RecommendFunctionalVidPn ddi adapter=0x%I64x request=0x%I64x",
               (ULONGLONG)(ULONG_PTR)hAdapter,
               (ULONGLONG)(ULONG_PTR)pRecommendFunctionalVidPn);
    NTSTATUS Status = pBDD->RecommendFunctionalVidPn(pRecommendFunctionalVidPn);
    BDD_PROBE1("RecommendFunctionalVidPn ddi result status=0x%08x", Status);
    return Status;
}

NTSTATUS
APIENTRY
BddDdiRecommendVidPnTopology(
    _In_ CONST HANDLE                                 hAdapter,
    _In_ CONST DXGKARG_RECOMMENDVIDPNTOPOLOGY* CONST  pRecommendVidPnTopology)
{
    PAGED_CODE();
    BDD_ASSERT_CHK(hAdapter != NULL);

    BASIC_DISPLAY_DRIVER* pBDD = reinterpret_cast<BASIC_DISPLAY_DRIVER*>(hAdapter);
    if (!pBDD->IsDriverActive())
    {
        BDD_LOG_ASSERTION1("BDD (0x%I64x) is being called when not active!", pBDD);
        return STATUS_UNSUCCESSFUL;
    }
    return pBDD->RecommendVidPnTopology(pRecommendVidPnTopology);
}

NTSTATUS
APIENTRY
BddDdiRecommendMonitorModes(
    _In_ CONST HANDLE                                hAdapter,
    _In_ CONST DXGKARG_RECOMMENDMONITORMODES* CONST  pRecommendMonitorModes)
{
    PAGED_CODE();
    BDD_ASSERT_CHK(hAdapter != NULL);

    BASIC_DISPLAY_DRIVER* pBDD = reinterpret_cast<BASIC_DISPLAY_DRIVER*>(hAdapter);
    if (!pBDD->IsDriverActive())
    {
        BDD_LOG_ASSERTION1("BDD (0x%I64x) is being called when not active!", pBDD);
        return STATUS_UNSUCCESSFUL;
    }
    return pBDD->RecommendMonitorModes(pRecommendMonitorModes);
}

NTSTATUS
APIENTRY
BddDdiEnumVidPnCofuncModality(
    _In_ CONST HANDLE                                 hAdapter,
    _In_ CONST DXGKARG_ENUMVIDPNCOFUNCMODALITY* CONST pEnumCofuncModality)
{
    PAGED_CODE();
    BDD_ASSERT_CHK(hAdapter != NULL);

    BASIC_DISPLAY_DRIVER* pBDD = reinterpret_cast<BASIC_DISPLAY_DRIVER*>(hAdapter);
    if (!pBDD->IsDriverActive())
    {
        BDD_LOG_ASSERTION1("BDD (0x%I64x) is being called when not active!", pBDD);
        return STATUS_UNSUCCESSFUL;
    }
    return pBDD->EnumVidPnCofuncModality(pEnumCofuncModality);
}

NTSTATUS
APIENTRY
BddDdiSetVidPnSourceVisibility(
    _In_ CONST HANDLE                            hAdapter,
    _In_ CONST DXGKARG_SETVIDPNSOURCEVISIBILITY* pSetVidPnSourceVisibility)
{
    PAGED_CODE();
    BDD_ASSERT_CHK(hAdapter != NULL);

    BASIC_DISPLAY_DRIVER* pBDD = reinterpret_cast<BASIC_DISPLAY_DRIVER*>(hAdapter);
    if (!pBDD->IsDriverActive())
    {
        BDD_LOG_ASSERTION1("BDD (0x%I64x) is being called when not active!", pBDD);
        return STATUS_UNSUCCESSFUL;
    }
    BDD_PROBE3("SetVidPnSourceVisibility ddi adapter=0x%I64x sourceId=%u visible=%u",
               (ULONGLONG)(ULONG_PTR)hAdapter,
               (pSetVidPnSourceVisibility != NULL) ? pSetVidPnSourceVisibility->VidPnSourceId : 0,
               (pSetVidPnSourceVisibility != NULL) ? pSetVidPnSourceVisibility->Visible : 0);
    NTSTATUS Status = pBDD->SetVidPnSourceVisibility(pSetVidPnSourceVisibility);
    BDD_PROBE1("SetVidPnSourceVisibility ddi result status=0x%08x", Status);
    return Status;
}

NTSTATUS
APIENTRY
BddDdiCommitVidPn(
    _In_ CONST HANDLE                     hAdapter,
    _In_ CONST DXGKARG_COMMITVIDPN* CONST pCommitVidPn)
{
    PAGED_CODE();
    BDD_ASSERT_CHK(hAdapter != NULL);

    BASIC_DISPLAY_DRIVER* pBDD = reinterpret_cast<BASIC_DISPLAY_DRIVER*>(hAdapter);
    if (!pBDD->IsDriverActive())
    {
        BDD_LOG_ASSERTION1("BDD (0x%I64x) is being called when not active!", pBDD);
        return STATUS_UNSUCCESSFUL;
    }
    BDD_PROBE3("CommitVidPn ddi adapter=0x%I64x functionalVidPn=0x%I64x affectedSource=%u",
               (ULONGLONG)(ULONG_PTR)hAdapter,
               (pCommitVidPn != NULL) ? (ULONGLONG)(ULONG_PTR)pCommitVidPn->hFunctionalVidPn : 0,
               (pCommitVidPn != NULL) ? pCommitVidPn->AffectedVidPnSourceId : 0);
    NTSTATUS Status = pBDD->CommitVidPn(pCommitVidPn);
    BDD_PROBE1("CommitVidPn ddi result status=0x%08x", Status);
    return Status;
}

NTSTATUS
APIENTRY
BddDdiUpdateActiveVidPnPresentPath(
    _In_ CONST HANDLE                                      hAdapter,
    _In_ CONST DXGKARG_UPDATEACTIVEVIDPNPRESENTPATH* CONST pUpdateActiveVidPnPresentPath)
{
    PAGED_CODE();
    BDD_ASSERT_CHK(hAdapter != NULL);

    BASIC_DISPLAY_DRIVER* pBDD = reinterpret_cast<BASIC_DISPLAY_DRIVER*>(hAdapter);
    if (!pBDD->IsDriverActive())
    {
        BDD_LOG_ASSERTION1("BDD (0x%I64x) is being called when not active!", pBDD);
        return STATUS_UNSUCCESSFUL;
    }
    return pBDD->UpdateActiveVidPnPresentPath(pUpdateActiveVidPnPresentPath);
}

NTSTATUS
APIENTRY
BddDdiQueryVidPnHWCapability(
    _In_ CONST HANDLE                       hAdapter,
    _Inout_ DXGKARG_QUERYVIDPNHWCAPABILITY* pVidPnHWCaps)
{
    PAGED_CODE();
    BDD_ASSERT_CHK(hAdapter != NULL);

    BASIC_DISPLAY_DRIVER* pBDD = reinterpret_cast<BASIC_DISPLAY_DRIVER*>(hAdapter);
    if (!pBDD->IsDriverActive())
    {
        BDD_LOG_ASSERTION1("BDD (0x%I64x) is being called when not active!", pBDD);
        return STATUS_UNSUCCESSFUL;
    }
    return pBDD->QueryVidPnHWCapability(pVidPnHWCaps);
}
//END: Paged Code
#pragma code_seg(pop)

#pragma code_seg(push)
#pragma code_seg()
// BEGIN: Non-Paged Code

VOID
BddDdiDpcRoutine(
    _In_  VOID* pDeviceContext)
{
    BDD_ASSERT_CHK(pDeviceContext != NULL);

    BASIC_DISPLAY_DRIVER* pBDD = reinterpret_cast<BASIC_DISPLAY_DRIVER*>(pDeviceContext);
    if (!pBDD->IsDriverActive())
    {
        BDD_LOG_ASSERTION1("BDD (0x%I64x) is being called when not active!", pBDD);
        return;
    }
    BDD_PROBE1("DpcRoutine ddi context=0x%I64x", (ULONGLONG)(ULONG_PTR)pBDD);
    pBDD->DpcRoutine();
}

BOOLEAN
BddDdiInterruptRoutine(
    _In_  VOID* pDeviceContext,
    _In_  ULONG MessageNumber)
{
    BDD_ASSERT_CHK(pDeviceContext != NULL);

    BASIC_DISPLAY_DRIVER* pBDD = reinterpret_cast<BASIC_DISPLAY_DRIVER*>(pDeviceContext);
    BDD_PROBE2("InterruptRoutine ddi context=0x%I64x message=%u",
               (ULONGLONG)(ULONG_PTR)pBDD,
               MessageNumber);
    BOOLEAN Claimed = pBDD->InterruptRoutine(MessageNumber);
    BDD_PROBE1("InterruptRoutine ddi claimed=%u", Claimed);
    return Claimed;
}

VOID
BddDdiResetDevice(
    _In_  VOID* pDeviceContext)
{
    BDD_ASSERT_CHK(pDeviceContext != NULL);

    BASIC_DISPLAY_DRIVER* pBDD = reinterpret_cast<BASIC_DISPLAY_DRIVER*>(pDeviceContext);
    pBDD->ResetDevice();
}

NTSTATUS
APIENTRY
BddDdiSystemDisplayEnable(
    _In_  VOID* pDeviceContext,
    _In_  D3DDDI_VIDEO_PRESENT_TARGET_ID TargetId,
    _In_  PDXGKARG_SYSTEM_DISPLAY_ENABLE_FLAGS Flags,
    _Out_ UINT* Width,
    _Out_ UINT* Height,
    _Out_ D3DDDIFORMAT* ColorFormat)
{
    BDD_ASSERT_CHK(pDeviceContext != NULL);

    BASIC_DISPLAY_DRIVER* pBDD = reinterpret_cast<BASIC_DISPLAY_DRIVER*>(pDeviceContext);
    return pBDD->SystemDisplayEnable(TargetId, Flags, Width, Height, ColorFormat);
}

VOID
APIENTRY
BddDdiSystemDisplayWrite(
    _In_  VOID* pDeviceContext,
    _In_  VOID* Source,
    _In_  UINT  SourceWidth,
    _In_  UINT  SourceHeight,
    _In_  UINT  SourceStride,
    _In_  UINT  PositionX,
    _In_  UINT  PositionY)
{
    BDD_ASSERT_CHK(pDeviceContext != NULL);

    BASIC_DISPLAY_DRIVER* pBDD = reinterpret_cast<BASIC_DISPLAY_DRIVER*>(pDeviceContext);
    pBDD->SystemDisplayWrite(Source, SourceWidth, SourceHeight, SourceStride, PositionX, PositionY);
}

// END: Non-Paged Code
#pragma code_seg(pop)

