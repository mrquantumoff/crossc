/* Prints the MSVC layout of the bthddi types/constants crossc-bth relies on.
 * Build + run: build.cmd (any non-elevated prompt).
 * The output is frozen in msvc-layout.txt; src/bth.rs encodes the same
 * numbers as compile-time asserts against the bindgen-generated types, so the
 * driver build fails if bindgen's layout ever diverges from MSVC's. */
#define _AMD64_ 1
#define NTDDI_VERSION 0x0A000010
#define _WIN32_WINNT 0x0A00
#include <ntddk.h>
#include <initguid.h>
#include <bthdef.h>
#include <bthioctl.h>
#include <bthddi.h>
#include <bthguid.h>
#include <stdio.h>

#define S(t) printf("sizeof %s = %u\n", #t, (unsigned)sizeof(t))
#define O(t, f) printf("offsetof %s.%s = %u\n", #t, #f, (unsigned)FIELD_OFFSET(t, f))

int main(void) {
    printf("IOCTL_INTERNAL_BTH_SUBMIT_BRB = 0x%08X\n", (unsigned)IOCTL_INTERNAL_BTH_SUBMIT_BRB);
    printf("IOCTL_INTERNAL_BTHENUM_GET_DEVINFO = 0x%08X\n", (unsigned)IOCTL_INTERNAL_BTHENUM_GET_DEVINFO);
    printf("IOCTL_INTERNAL_BTHENUM_GET_ENUMINFO = 0x%08X\n", (unsigned)IOCTL_INTERNAL_BTHENUM_GET_ENUMINFO);
    {
        const GUID *g = &GUID_BTHDDI_PROFILE_DRIVER_INTERFACE;
        printf("GUID_BTHDDI_PROFILE_DRIVER_INTERFACE = %08X-%04X-%04X-%02X%02X-%02X%02X%02X%02X%02X%02X\n",
               (unsigned)g->Data1, g->Data2, g->Data3, g->Data4[0], g->Data4[1], g->Data4[2], g->Data4[3],
               g->Data4[4], g->Data4[5], g->Data4[6], g->Data4[7]);
    }
    printf("BTHDDI_PROFILE_DRIVER_INTERFACE_VERSION_FOR_QI = 0x%04X\n", BTHDDI_PROFILE_DRIVER_INTERFACE_VERSION_FOR_QI);
    S(BRB_HEADER); S(BRB); S(struct _BRB_L2CA_OPEN_CHANNEL); S(struct _BRB_L2CA_CLOSE_CHANNEL);
    S(struct _BRB_L2CA_ACL_TRANSFER); S(BTH_DEVICE_INFO); S(BTH_PROFILE_DRIVER_INTERFACE); S(INDICATION_PARAMETERS);
    O(BRB_HEADER, Length); O(BRB_HEADER, Type); O(BRB_HEADER, Status); O(BRB_HEADER, BtStatus); O(BRB_HEADER, ClientContext);
    O(struct _BRB_L2CA_OPEN_CHANNEL, ChannelHandle); O(struct _BRB_L2CA_OPEN_CHANNEL, Psm);
    O(struct _BRB_L2CA_OPEN_CHANNEL, ChannelFlags); O(struct _BRB_L2CA_OPEN_CHANNEL, BtAddress);
    O(struct _BRB_L2CA_OPEN_CHANNEL, ConfigOut); O(struct _BRB_L2CA_OPEN_CHANNEL, ConfigIn);
    O(struct _BRB_L2CA_OPEN_CHANNEL, CallbackFlags); O(struct _BRB_L2CA_OPEN_CHANNEL, Callback);
    O(struct _BRB_L2CA_OPEN_CHANNEL, CallbackContext); O(struct _BRB_L2CA_OPEN_CHANNEL, ReferenceObject);
    O(struct _BRB_L2CA_OPEN_CHANNEL, OutResults); O(struct _BRB_L2CA_OPEN_CHANNEL, InResults);
    O(struct _BRB_L2CA_OPEN_CHANNEL, IncomingQueueDepth);
    O(struct _BRB_L2CA_ACL_TRANSFER, ChannelHandle); O(struct _BRB_L2CA_ACL_TRANSFER, TransferFlags);
    O(struct _BRB_L2CA_ACL_TRANSFER, BufferSize); O(struct _BRB_L2CA_ACL_TRANSFER, Buffer);
    O(struct _BRB_L2CA_ACL_TRANSFER, BufferMDL); O(struct _BRB_L2CA_ACL_TRANSFER, Timeout);
    O(struct _BRB_L2CA_ACL_TRANSFER, RemainingBufferSize);
    O(struct _BRB_L2CA_CLOSE_CHANNEL, BtAddress); O(struct _BRB_L2CA_CLOSE_CHANNEL, ChannelHandle);
    O(BTH_DEVICE_INFO, flags); O(BTH_DEVICE_INFO, address); O(BTH_DEVICE_INFO, classOfDevice); O(BTH_DEVICE_INFO, name);
    O(BTH_PROFILE_DRIVER_INTERFACE, BthAllocateBrb); O(BTH_PROFILE_DRIVER_INTERFACE, BthFreeBrb);
    O(BTH_PROFILE_DRIVER_INTERFACE, BthInitializeBrb); O(BTH_PROFILE_DRIVER_INTERFACE, BthReuseBrb);
    O(INDICATION_PARAMETERS, ConnectionHandle); O(INDICATION_PARAMETERS, BtAddress); O(INDICATION_PARAMETERS, Parameters);
    return 0;
}
