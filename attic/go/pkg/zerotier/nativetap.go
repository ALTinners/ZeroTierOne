/*
 * Copyright (C)2013-2020 ZeroTier, Inc.
 *
 * Use of this software is governed by the Business Source License included
 * in the LICENSE.TXT file in the project's root directory.
 *
 * Change Date: 2026-01-01
 *
 * On the date above, in accordance with the Business Source License, use
 * of this software will be governed by version 2.0 of the Apache License.
 */
/****/

// This wraps the C++ EthernetTap and its implementations.

package zerotier

//#include "../../serviceiocore/GoGlue.h"
import "C"

import (
	"fmt"
	"net"
	"sync"
	"sync/atomic"
	"syscall"
	"unsafe"
)

// nativeTap is a Tap implementation that wraps a native C++ interface to a system tun/tap device
type nativeTap struct {
	tap                        unsafe.Pointer
	networkStatus              uint32
	enabled                    uint32
	multicastGroupHandlers     []func(bool, *MulticastGroup)
	multicastGroupHandlersLock sync.Mutex
}

// Close is a no-op for the native tap because GoGlue does this when networks are left
func (t *nativeTap) Close() {}

// Type returns a human-readable description of this tap implementation
func (t *nativeTap) Type() string {
	return "native"
}

// Error gets this tap device's error status
func (t *nativeTap) Error() (int, string) {
	return 0, ""
}

// SetEnabled sets this tap's enabled state
func (t *nativeTap) SetEnabled(enabled bool) {
	if enabled && atomic.SwapUint32(&t.enabled, 1) == 0 {
		C.ZT_GoTap_setEnabled(t.tap, 1)
	} else if !enabled && atomic.SwapUint32(&t.enabled, 0) == 1 {
		C.ZT_GoTap_setEnabled(t.tap, 0)
	}
}

// Enabled returns true if this tap is currently processing packets
func (t *nativeTap) Enabled() bool {
	return atomic.LoadUint32(&t.enabled) != 0
}

// AddIP adds an IP address (with netmask) to this tap
func (t *nativeTap) AddIP(ip *InetAddress) error {
	if len(ip.IP) == 16 {
		if ip.Port > 128 || ip.Port < 0 {
			return ErrInvalidParameter
		}
		C.ZT_GoTap_addIp(t.tap, C.int(syscall.AF_INET6), unsafe.Pointer(&ip.IP[0]), C.int(ip.Port))
	} else if len(ip.IP) == 4 {
		if ip.Port > 32 || ip.Port < 0 {
			return ErrInvalidParameter
		}
		C.ZT_GoTap_addIp(t.tap, C.int(syscall.AF_INET), unsafe.Pointer(&ip.IP[0]), C.int(ip.Port))
	}
	return ErrInvalidParameter
}

// RemoveIP removes this IP address (with netmask) from this tap
func (t *nativeTap) RemoveIP(ip *InetAddress) error {
	if len(ip.IP) == 16 {
		if ip.Port > 128 || ip.Port < 0 {
			return ErrInvalidParameter
		}
		C.ZT_GoTap_removeIp(t.tap, C.int(syscall.AF_INET6), unsafe.Pointer(&ip.IP[0]), C.int(ip.Port))
		return nil
	}
	if len(ip.IP) == 4 {
		if ip.Port > 32 || ip.Port < 0 {
			return ErrInvalidParameter
		}
		C.ZT_GoTap_removeIp(t.tap, C.int(syscall.AF_INET), unsafe.Pointer(&ip.IP[0]), C.int(ip.Port))
		return nil
	}
	return ErrInvalidParameter
}

// IPs returns IPs currently assigned to this tap (including externally or system-assigned IPs)
func (t *nativeTap) IPs() (ips []net.IPNet, err error) {
	defer func() {
		e := recover()
		if e != nil {
			err = fmt.Errorf("%v", e)
		}
	}()
	var ipbuf [16384]byte
	count := int(C.ZT_GoTap_ips(t.tap, unsafe.Pointer(&ipbuf[0]), 16384))
	ipptr := 0
	for i := 0; i < count; i++ {
		af := int(ipbuf[ipptr])
		ipptr++
		switch af {
		case syscall.AF_INET:
			var ip [4]byte
			for j := 0; j < 4; j++ {
				ip[j] = ipbuf[ipptr]
				ipptr++
			}
			bits := ipbuf[ipptr]
			ipptr++
			ips = append(ips, net.IPNet{IP: net.IP(ip[:]), Mask: net.CIDRMask(int(bits), 32)})
		case syscall.AF_INET6:
			var ip [16]byte
			for j := 0; j < 16; j++ {
				ip[j] = ipbuf[ipptr]
				ipptr++
			}
			bits := ipbuf[ipptr]
			ipptr++
			ips = append(ips, net.IPNet{IP: net.IP(ip[:]), Mask: net.CIDRMask(int(bits), 128)})
		}
	}
	return
}

// DeviceName gets this tap's OS-specific device name
func (t *nativeTap) DeviceName() string {
	var dn [256]byte
	C.ZT_GoTap_deviceName(t.tap, (*C.char)(unsafe.Pointer(&dn[0])))
	for i, b := range dn {
		if b == 0 {
			return string(dn[0:i])
		}
	}
	return ""
}

// AddMulticastGroupChangeHandler adds a function to be called when the tap subscribes or unsubscribes to a multicast group.
func (t *nativeTap) AddMulticastGroupChangeHandler(handler func(bool, *MulticastGroup)) {
	t.multicastGroupHandlersLock.Lock()
	t.multicastGroupHandlers = append(t.multicastGroupHandlers, handler)
	t.multicastGroupHandlersLock.Unlock()
}

func handleTapMulticastGroupChange(gn unsafe.Pointer, nwid, mac C.uint64_t, adi C.uint32_t, added bool) {
	node := cNodeRefs[uintptr(gn)]
	if node == nil {
		return
	}
	node.networksLock.RLock()
	network := node.networks[NetworkID(nwid)]
	node.networksLock.RUnlock()
	if network == nil {
		return
	}

	node.runWaitGroup.Add(1)
	go func() {
		defer node.runWaitGroup.Done()
		tap, _ := network.tap.(*nativeTap)
		if tap != nil {
			mg := &MulticastGroup{MAC: MAC(mac), ADI: uint32(adi)}
			tap.multicastGroupHandlersLock.Lock()
			defer tap.multicastGroupHandlersLock.Unlock()
			for _, h := range tap.multicastGroupHandlers {
				h(added, mg)
			}
		}
	}()
}

//export goHandleTapAddedMulticastGroup
func goHandleTapAddedMulticastGroup(gn, _ unsafe.Pointer, nwid, mac C.uint64_t, adi C.uint32_t) {
	handleTapMulticastGroupChange(gn, nwid, mac, adi, true)
}

//export goHandleTapRemovedMulticastGroup
func goHandleTapRemovedMulticastGroup(gn, _ unsafe.Pointer, nwid, mac C.uint64_t, adi C.uint32_t) {
	handleTapMulticastGroupChange(gn, nwid, mac, adi, false)
}
