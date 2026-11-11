/* Prints the MSVC view of everything src/acx.rs relies on: ACX function-table
 * indices, struct layouts, enum values, what the ACX_*_INIT helpers write, and
 * the GUIDs / KS wave-format layout that are hand-written in Rust.
 * Build + run: build.cmd (any non-elevated prompt).
 * The output is frozen in msvc-layout.txt; the `const _` block in src/acx.rs
 * encodes the same numbers as compile-time asserts against the bindgen types,
 * so the driver build fails if bindgen (clang) and MSVC ever disagree, or if a
 * hand-written constant drifts from the headers. Same idea as tools/bthcheck.
 *
 * This is a user-mode console program that only *compiles* the kernel headers;
 * the WDF / ACX tables the inline helpers reference are defined here as dummies
 * so it links. Nothing is called through them. */
#define _AMD64_ 1
#define NTDDI_VERSION 0x0A000010
#define _WIN32_WINNT 0x0A00
#define KMDF_VERSION_MAJOR 1
#define KMDF_VERSION_MINOR 33
#define ACX_VERSION_MAJOR 1
#define ACX_VERSION_MINOR 1
#include <ntddk.h>
#include <wdf.h>
#include <initguid.h>
#include <windef.h>
#include <ks.h>
#include <mmsystem.h>
#include <ksmedia.h>
#include <acx.h>
#include <devpkey.h>
#include <stdio.h>

/* dummies for the symbols the FORCEINLINE helpers reference */
PWDF_DRIVER_GLOBALS WdfDriverGlobals;
BOOLEAN WdfClientVersionHigherThanFramework;
ULONG WdfStructureCount;

WDF_STRUCT_INFO WdfStructures;
ACXFUNC AcxFunctions[AcxFunctionTableNumEntries];
PACX_DRIVER_GLOBALS AcxDriverGlobals;
BOOLEAN AcxClientVersionHigherThanFramework;
ULONG AcxStructureCount;

WDF_STRUCT_INFO AcxStructures;

#define S(t) printf("sizeof %s = %u\n", #t, (unsigned)sizeof(t))
#define O(t, f) printf("offsetof %s.%s = %u\n", #t, #f, (unsigned)FIELD_OFFSET(t, f))
#define I(n) printf("index %s = %d\n", #n, (int)n##TableIndex)
#define E(n) printf("enum %s = %d\n", #n, (int)(n))
#define V(n) printf("value %s = 0x%X\n", #n, (unsigned)(n))
#define G(n) { static const GUID g = { STATIC_##n }; pguid(#n, &g); }

static void pguid(const char *name, const GUID *g) {
    printf("guid %s = %08X-%04X-%04X-%02X%02X-%02X%02X%02X%02X%02X%02X\n", name, (unsigned)g->Data1, g->Data2,
           g->Data3, g->Data4[0], g->Data4[1], g->Data4[2], g->Data4[3], g->Data4[4], g->Data4[5], g->Data4[6],
           g->Data4[7]);
}

int main(void) {
    printf("ACX %d.%d minimum %d, KMDF %d.%d\n", ACX_VERSION_MAJOR, ACX_VERSION_MINOR, (int)ACX_MINIMUM_VERSION_REQUIRED,
           KMDF_VERSION_MAJOR, KMDF_VERSION_MINOR);
    printf("index AcxFunctionTableNumEntries = %d\n", (int)AcxFunctionTableNumEntries);
    I(AcxCircuitInitAllocate); I(AcxCircuitInitFree); I(AcxCircuitInitSetComponentId); I(AcxCircuitInitAssignName);
    I(AcxCircuitInitSetCircuitType); I(AcxCircuitInitSetAcxCircuitPnpPowerCallbacks);
    I(AcxCircuitInitAssignAcxCreateStreamCallback); I(AcxCircuitCreate); I(AcxCircuitAddPins);
    I(AcxDataFormatCreate); I(AcxDataFormatGetChannelsCount); I(AcxDataFormatGetSampleRate);
    I(AcxDataFormatGetBitsPerSample); I(AcxDataFormatGetBlockAlign); I(AcxDataFormatGetAverageBytesPerSec);
    I(AcxDataFormatListAddDataFormat); I(AcxDataFormatListAssignDefaultDataFormat);
    I(AcxDeviceInitInitialize); I(AcxDeviceInitialize); I(AcxDeviceAddCircuit); I(AcxDeviceRemoveCircuit);
    I(AcxDriverInitialize); I(AcxPinCreate); I(AcxPinAddJacks); I(AcxPinGetRawDataFormatList); I(AcxJackCreate);
    I(AcxStreamInitAssignAcxStreamCallbacks); I(AcxStreamInitAssignAcxRtStreamCallbacks);
    I(AcxStreamInitSetAcxRtStreamSupportsNotifications); I(AcxRtStreamCreate); I(AcxRtStreamNotifyPacketComplete);

    S(ACXFUNC); S(ACX_DRIVER_CONFIG);
    S(ACX_DEVICEINIT_CONFIG); O(ACX_DEVICEINIT_CONFIG, SynchronizationScope); O(ACX_DEVICEINIT_CONFIG, ExecutionLevel);
    S(ACX_DEVICE_CONFIG);
    S(ACX_CIRCUIT_PNPPOWER_CALLBACKS); O(ACX_CIRCUIT_PNPPOWER_CALLBACKS, EvtAcxCircuitPowerUp);
    O(ACX_CIRCUIT_PNPPOWER_CALLBACKS, EvtAcxCircuitPowerDown);
    S(ACX_PIN_CALLBACKS); O(ACX_PIN_CALLBACKS, EvtAcxPinSetDataFormat);
    S(ACX_PIN_CONFIG); O(ACX_PIN_CONFIG, Flags); O(ACX_PIN_CONFIG, Id); O(ACX_PIN_CONFIG, Type);
    O(ACX_PIN_CONFIG, Communication); O(ACX_PIN_CONFIG, Category); O(ACX_PIN_CONFIG, PinCallbacks);
    O(ACX_PIN_CONFIG, MaxStreams);
    S(ACX_JACK_CONFIG); O(ACX_JACK_CONFIG, Id); O(ACX_JACK_CONFIG, Flags); O(ACX_JACK_CONFIG, Callbacks);
    O(ACX_JACK_CONFIG, Description);
    S(ACX_JACK_DESCRIPTION); O(ACX_JACK_DESCRIPTION, ConnectionType); O(ACX_JACK_DESCRIPTION, PortConnection);
    S(ACX_DATAFORMAT_CONFIG); O(ACX_DATAFORMAT_CONFIG, Type); O(ACX_DATAFORMAT_CONFIG, u);
    S(ACX_STREAM_CALLBACKS); O(ACX_STREAM_CALLBACKS, EvtAcxStreamPrepareHardware);
    O(ACX_STREAM_CALLBACKS, EvtAcxStreamReleaseHardware); O(ACX_STREAM_CALLBACKS, EvtAcxStreamRun);
    O(ACX_STREAM_CALLBACKS, EvtAcxStreamPause);
    S(ACX_RT_STREAM_CALLBACKS); O(ACX_RT_STREAM_CALLBACKS, EvtAcxStreamGetHwLatency);
    O(ACX_RT_STREAM_CALLBACKS, EvtAcxStreamAllocateRtPackets); O(ACX_RT_STREAM_CALLBACKS, EvtAcxStreamFreeRtPackets);
    O(ACX_RT_STREAM_CALLBACKS, EvtAcxStreamSetRenderPacket); O(ACX_RT_STREAM_CALLBACKS, EvtAcxStreamGetCurrentPacket);
    O(ACX_RT_STREAM_CALLBACKS, EvtAcxStreamGetCapturePacket);
    O(ACX_RT_STREAM_CALLBACKS, EvtAcxStreamGetPresentationPosition);
    S(ACX_RTPACKET); O(ACX_RTPACKET, RtPacketBuffer); O(ACX_RTPACKET, RtPacketOffset); O(ACX_RTPACKET, RtPacketSize);
    S(WDF_MEMORY_DESCRIPTOR); O(WDF_MEMORY_DESCRIPTOR, u);
    S(WDF_TIMER_CONFIG); O(WDF_TIMER_CONFIG, UseHighResolutionTimer);

    E(AcxCircuitTypeRender); E(AcxPinTypeSink); E(AcxPinTypeSource); E(AcxPinCommunicationNone);
    E(AcxPinCommunicationSink); E(AcxDataFormatKsFormat); E(AcxConnTypeOtherDigital); E(AcxGeoLocNotApplicable);
    E(AcxGenLocSeparate); E(AcxPortConnIntegratedDevice); E(AcxStreamStateStop); E(AcxStreamStatePause);
    E(AcxStreamStateRun); E(WdfSynchronizationScopeNone); E(WdfExecutionLevelPassive);
    E(WdfMemoryDescriptorTypeMdl);

    V(AcxPinIdDefault); V(AcxJackPropertiesIdDefault); V(AcxInstanceIndeterminate); V(WAVE_FORMAT_EXTENSIBLE);
    V(SPEAKER_FRONT_LEFT); V(SPEAKER_FRONT_RIGHT); V(KSAUDIO_SPEAKER_STEREO); V(PAGE_SIZE);
    V(POOL_FLAG_NON_PAGED); V(PLUGPLAY_REGKEY_DEVICE); V(KEY_READ);
    V(STATUS_DATA_LATE_ERROR); V(STATUS_DATA_OVERRUN); V(STATUS_INVALID_STATE_TRANSITION);

    /* ---- audio path (jack presence, tap, audio IOCTL queues, device name) ---- */
    I(AcxJackChangeStateNotification); I(AcxDataFormatGetValidBitsPerSample);
    S(ACX_JACK_CALLBACKS); O(ACX_JACK_CALLBACKS, EvtAcxJackRetrievePresenceState);
    E(AcxJackConfigJackDetection); E(AcxPortConnJack);
    {
        ACX_JACK_CALLBACKS c; ACX_JACK_CALLBACKS_INIT(&c);
        printf("init ACX_JACK_CALLBACKS: Size=%u\n", c.Size);
    }
    S(WDF_WORKITEM_CONFIG); O(WDF_WORKITEM_CONFIG, EvtWorkItemFunc); O(WDF_WORKITEM_CONFIG, AutomaticSerialization);
    {
        WDF_WORKITEM_CONFIG c; WDF_WORKITEM_CONFIG_INIT(&c, NULL);
        printf("init WDF_WORKITEM_CONFIG: Size=%u AutomaticSerialization=%d\n", c.Size, (int)c.AutomaticSerialization);
    }
    S(WDF_IO_QUEUE_CONFIG); O(WDF_IO_QUEUE_CONFIG, DispatchType); O(WDF_IO_QUEUE_CONFIG, PowerManaged);
    O(WDF_IO_QUEUE_CONFIG, DefaultQueue); O(WDF_IO_QUEUE_CONFIG, EvtIoCanceledOnQueue);
    {
        WDF_IO_QUEUE_CONFIG c; WDF_IO_QUEUE_CONFIG_INIT(&c, WdfIoQueueDispatchManual);
        printf("init WDF_IO_QUEUE_CONFIG(manual): Size=%u DispatchType=%d PowerManaged=%d DefaultQueue=%d\n", c.Size,
               (int)c.DispatchType, (int)c.PowerManaged, (int)c.DefaultQueue);
    }
    E(WdfIoQueueDispatchManual); E(WdfFalse); E(WdfUseDefault); E(WdfExecutionLevelDispatch); E(KernelMode);
    S(WDF_DEVICE_PROPERTY_DATA); O(WDF_DEVICE_PROPERTY_DATA, PropertyKey); O(WDF_DEVICE_PROPERTY_DATA, Lcid);
    O(WDF_DEVICE_PROPERTY_DATA, Flags);
    S(DEVPROPKEY); O(DEVPROPKEY, pid);
    V(DEVPROP_TYPE_STRING); V(PLUGPLAY_PROPERTY_PERSISTENT);
    V(STATUS_DEVICE_BUSY); V(STATUS_ACCESS_DENIED); V(STATUS_NOT_SUPPORTED); V(STATUS_BUFFER_TOO_SMALL);
    V(STATUS_CANCELLED); V(STATUS_NO_MORE_ENTRIES);
    pguid("DEVPKEY_Device_FriendlyName.fmtid", &DEVPKEY_Device_FriendlyName.fmtid);
    printf("value DEVPKEY_Device_FriendlyName.pid = %u\n", (unsigned)DEVPKEY_Device_FriendlyName.pid);

    /* what the INIT helpers write (src/acx.rs builds the same structs by hand) */
    {
        ACX_DRIVER_CONFIG c; ACX_DRIVER_CONFIG_INIT(&c);
        printf("init ACX_DRIVER_CONFIG: Size=%u Flags=%u\n", c.Size, c.Flags);
    }
    {
        ACX_DEVICEINIT_CONFIG c; ACX_DEVICEINIT_CONFIG_INIT(&c);
        printf("init ACX_DEVICEINIT_CONFIG: Size=%u Flags=%u SynchronizationScope=%d ExecutionLevel=%d\n", c.Size, c.Flags,
               (int)c.SynchronizationScope, (int)c.ExecutionLevel);
    }
    {
        ACX_DEVICE_CONFIG c; ACX_DEVICE_CONFIG_INIT(&c);
        printf("init ACX_DEVICE_CONFIG: Size=%u Flags=%u\n", c.Size, c.Flags);
    }
    {
        ACX_CIRCUIT_PNPPOWER_CALLBACKS c; ACX_CIRCUIT_PNPPOWER_CALLBACKS_INIT(&c);
        printf("init ACX_CIRCUIT_PNPPOWER_CALLBACKS: Size=%u\n", c.Size);
    }
    {
        ACX_PIN_CALLBACKS c; ACX_PIN_CALLBACKS_INIT(&c);
        printf("init ACX_PIN_CALLBACKS: Size=%u\n", c.Size);
    }
    {
        ACX_PIN_CONFIG c; ACX_PIN_CONFIG_INIT(&c);
        printf("init ACX_PIN_CONFIG: Size=%u Flags=%u Id=0x%X Type=%d Communication=%d Qualifier=%d MaxStreams=0x%X\n",
               c.Size, c.Flags, c.Id, (int)c.Type, (int)c.Communication, (int)c.Qualifier, c.MaxStreams);
    }
    {
        ACX_JACK_CONFIG c; ACX_JACK_CONFIG_INIT(&c);
        printf("init ACX_JACK_CONFIG: Size=%u Id=0x%X Flags=%u\n", c.Size, c.Id, c.Flags);
    }
    {
        static int dummy;
        ACX_DATAFORMAT_CONFIG c; ACX_DATAFORMAT_CONFIG_INIT_KS(&c, &dummy);
        printf("init ACX_DATAFORMAT_CONFIG(KS): Size=%u Flags=%u Type=%d KsFormat-at-offset=%u\n", c.Size, c.Flags,
               (int)c.Type, (unsigned)((char *)&c.u.KsFormat - (char *)&c));
    }
    {
        ACX_STREAM_CALLBACKS c; ACX_STREAM_CALLBACKS_INIT(&c);
        printf("init ACX_STREAM_CALLBACKS: Size=%u\n", c.Size);
    }
    {
        ACX_RT_STREAM_CALLBACKS c; ACX_RT_STREAM_CALLBACKS_INIT(&c);
        printf("init ACX_RT_STREAM_CALLBACKS: Size=%u\n", c.Size);
    }
    {
        ACX_RTPACKET c; ACX_RTPACKET_INIT(&c);
        printf("init ACX_RTPACKET: Size=%u\n", c.Size);
    }
    {
        WDF_MEMORY_DESCRIPTOR d; WDF_MEMORY_DESCRIPTOR_INIT_MDL(&d, (PMDL)(ULONG_PTR)0x1000, 0x2000);
        printf("init WDF_MEMORY_DESCRIPTOR(MDL): Type=%d Mdl-at-offset=%u BufferLength-at-offset=%u\n", (int)d.Type,
               (unsigned)((char *)&d.u.MdlType.Mdl - (char *)&d), (unsigned)((char *)&d.u.MdlType.BufferLength - (char *)&d));
    }
    {
        WDF_TIMER_CONFIG t; WDF_TIMER_CONFIG_INIT(&t, NULL);
        printf("init WDF_TIMER_CONFIG: Size=%u Period=%u AutomaticSerialization=%d TolerableDelay=%u UseHighResolutionTimer=%d\n",
               t.Size, t.Period, (int)t.AutomaticSerialization, t.TolerableDelay, (int)t.UseHighResolutionTimer);
    }

    G(KSCATEGORY_AUDIO); G(KSCATEGORY_RENDER); G(KSCATEGORY_REALTIME); G(KSNODETYPE_HEADPHONES);
    G(KSDATAFORMAT_TYPE_AUDIO); G(KSDATAFORMAT_SUBTYPE_PCM); G(KSDATAFORMAT_SPECIFIER_WAVEFORMATEX);
    G(AUDIO_SIGNALPROCESSINGMODE_RAW);

    S(KSDATAFORMAT); S(WAVEFORMATEX); S(WAVEFORMATEXTENSIBLE); S(KSDATAFORMAT_WAVEFORMATEXTENSIBLE);
    O(KSDATAFORMAT_WAVEFORMATEXTENSIBLE, DataFormat.MajorFormat); O(KSDATAFORMAT_WAVEFORMATEXTENSIBLE, DataFormat.SubFormat);
    O(KSDATAFORMAT_WAVEFORMATEXTENSIBLE, DataFormat.Specifier);
    O(KSDATAFORMAT_WAVEFORMATEXTENSIBLE, WaveFormatExt.Format.wFormatTag);
    O(KSDATAFORMAT_WAVEFORMATEXTENSIBLE, WaveFormatExt.Format.nChannels);
    O(KSDATAFORMAT_WAVEFORMATEXTENSIBLE, WaveFormatExt.Format.nSamplesPerSec);
    O(KSDATAFORMAT_WAVEFORMATEXTENSIBLE, WaveFormatExt.Format.nAvgBytesPerSec);
    O(KSDATAFORMAT_WAVEFORMATEXTENSIBLE, WaveFormatExt.Format.nBlockAlign);
    O(KSDATAFORMAT_WAVEFORMATEXTENSIBLE, WaveFormatExt.Format.wBitsPerSample);
    O(KSDATAFORMAT_WAVEFORMATEXTENSIBLE, WaveFormatExt.Format.cbSize);
    O(KSDATAFORMAT_WAVEFORMATEXTENSIBLE, WaveFormatExt.Samples.wValidBitsPerSample);
    O(KSDATAFORMAT_WAVEFORMATEXTENSIBLE, WaveFormatExt.dwChannelMask);
    O(KSDATAFORMAT_WAVEFORMATEXTENSIBLE, WaveFormatExt.SubFormat);
    printf("value sizeof(WAVEFORMATEXTENSIBLE)-sizeof(WAVEFORMATEX) = %u\n",
           (unsigned)(sizeof(WAVEFORMATEXTENSIBLE) - sizeof(WAVEFORMATEX)));
    printf("alignof KSDATAFORMAT_WAVEFORMATEXTENSIBLE = %u\n", (unsigned)__alignof(KSDATAFORMAT_WAVEFORMATEXTENSIBLE));

    /* ---- unified volume (AVRCP volume contract, volume / mute elements) ---- */
    I(AcxCircuitInitAssignCategories); I(AcxCircuitInitAssignProperties); I(AcxCircuitInitAssignMethods);
    I(AcxCircuitAddEvents); I(AcxCircuitAddElements); I(AcxMuteCreate); I(AcxMuteChangeStateNotification);
    I(AcxVolumeCreate); I(AcxVolumeChangeLevelNotification); I(AcxEventCreate); I(AcxEventGenerateEvent);
    I(AcxRequestGetParameters);
    S(ACX_PROPERTY_ITEM); O(ACX_PROPERTY_ITEM, Id); O(ACX_PROPERTY_ITEM, Flags);
    O(ACX_PROPERTY_ITEM, EvtAcxObjectProcessRequest); O(ACX_PROPERTY_ITEM, Reserved); O(ACX_PROPERTY_ITEM, ControlCb);
    O(ACX_PROPERTY_ITEM, ValueCb); O(ACX_PROPERTY_ITEM, ValueType);
    S(ACX_METHOD_ITEM); O(ACX_METHOD_ITEM, Id); O(ACX_METHOD_ITEM, Flags); O(ACX_METHOD_ITEM, EvtAcxObjectProcessRequest);
    O(ACX_METHOD_ITEM, Reserved); O(ACX_METHOD_ITEM, ArgsCb); O(ACX_METHOD_ITEM, ResultCb); O(ACX_METHOD_ITEM, ResultType);
    S(ACX_EVENT_CALLBACKS); O(ACX_EVENT_CALLBACKS, EvtAcxEventEnable); O(ACX_EVENT_CALLBACKS, EvtAcxEventDisable);
    O(ACX_EVENT_CALLBACKS, EvtAcxEventProcessRequest);
    S(ACX_EVENT_CONFIG); O(ACX_EVENT_CONFIG, Flags); O(ACX_EVENT_CONFIG, Set); O(ACX_EVENT_CONFIG, Id);
    O(ACX_EVENT_CONFIG, Type); O(ACX_EVENT_CONFIG, Callbacks);
    S(ACX_VOLUME_CALLBACKS); O(ACX_VOLUME_CALLBACKS, EvtAcxVolumeAssignLevel);
    O(ACX_VOLUME_CALLBACKS, EvtAcxRampedVolumeAssignLevel); O(ACX_VOLUME_CALLBACKS, EvtAcxVolumeRetrieveLevel);
    O(ACX_VOLUME_CALLBACKS, EvtAcxVolumeProcessRequest);
    S(ACX_VOLUME_CONFIG); O(ACX_VOLUME_CONFIG, Id); O(ACX_VOLUME_CONFIG, Name); O(ACX_VOLUME_CONFIG, Flags);
    O(ACX_VOLUME_CONFIG, ChannelsCount); O(ACX_VOLUME_CONFIG, Maximum); O(ACX_VOLUME_CONFIG, Minimum);
    O(ACX_VOLUME_CONFIG, SteppingDelta); O(ACX_VOLUME_CONFIG, Callbacks);
    S(ACX_MUTE_CALLBACKS); O(ACX_MUTE_CALLBACKS, EvtAcxMuteAssignState); O(ACX_MUTE_CALLBACKS, EvtAcxMuteRetrieveState);
    O(ACX_MUTE_CALLBACKS, EvtAcxMuteProcessRequest);
    S(ACX_MUTE_CONFIG); O(ACX_MUTE_CONFIG, Id); O(ACX_MUTE_CONFIG, Name); O(ACX_MUTE_CONFIG, Flags);
    O(ACX_MUTE_CONFIG, ChannelsCount); O(ACX_MUTE_CONFIG, Callbacks);
    S(ACX_REQUEST_PARAMETERS); O(ACX_REQUEST_PARAMETERS, Type); O(ACX_REQUEST_PARAMETERS, Parameters);
    O(ACX_REQUEST_PARAMETERS, Parameters.Property.Set); O(ACX_REQUEST_PARAMETERS, Parameters.Property.Id);
    O(ACX_REQUEST_PARAMETERS, Parameters.Property.Verb); O(ACX_REQUEST_PARAMETERS, Parameters.Property.Value);
    O(ACX_REQUEST_PARAMETERS, Parameters.Property.ValueCb); O(ACX_REQUEST_PARAMETERS, Parameters.Method.Id);
    E(AcxRequestTypeProperty); E(AcxRequestTypeMethod); E(AcxPropertyVerbGet); E(AcxPropertyVerbSet);
    V(ACX_PROPERTY_ITEM_FLAG_GET); V(ACX_PROPERTY_ITEM_FLAG_SET); V(ACX_METHOD_ITEM_FLAG_SEND); V(AcxElementIdDefault);
    V(STATUS_INVALID_DEVICE_STATE); V(STATUS_INVALID_DEVICE_REQUEST); V(STATUS_INVALID_PARAMETER);
    {
        ACX_VOLUME_CONFIG c; ACX_VOLUME_CONFIG_INIT(&c);
        printf("init ACX_VOLUME_CONFIG: Size=%u Id=0x%X Flags=%u\n", c.Size, c.Id, c.Flags);
    }
    {
        ACX_MUTE_CONFIG c; ACX_MUTE_CONFIG_INIT(&c);
        printf("init ACX_MUTE_CONFIG: Size=%u Id=0x%X Flags=%u\n", c.Size, c.Id, c.Flags);
    }
    {
        ACX_EVENT_CONFIG c; ACX_EVENT_CONFIG_INIT(&c);
        printf("init ACX_EVENT_CONFIG: Size=%u Flags=%u Type=%u\n", c.Size, c.Flags, c.Type);
    }
    {
        ACX_VOLUME_CALLBACKS v; ACX_MUTE_CALLBACKS m; ACX_EVENT_CALLBACKS e;
        ACX_VOLUME_CALLBACKS_INIT(&v); ACX_MUTE_CALLBACKS_INIT(&m); ACX_EVENT_CALLBACKS_INIT(&e);
        printf("init ACX_VOLUME_CALLBACKS / ACX_MUTE_CALLBACKS / ACX_EVENT_CALLBACKS: Size=%u / %u / %u\n", v.Size, m.Size,
               e.Size);
    }
    {
        ACX_REQUEST_PARAMETERS r; ACX_REQUEST_PARAMETERS_INIT(&r);
        printf("init ACX_REQUEST_PARAMETERS: Size=%u\n", (unsigned)r.Size);
    }
    G(KSCATEGORY_CAPTURE); G(KSCATEGORY_TOPOLOGY); G(KSAUDFNAME_VOLUME_CONTROL); G(KSAUDFNAME_WAVE_MUTE);
    return 0;
}
