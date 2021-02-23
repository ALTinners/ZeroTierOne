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

package zerotier

import (
	"encoding/binary"
	"encoding/json"
	"fmt"
	"net"
	"sort"
	"strconv"
	"sync"
)

// NetworkID is a network's 64-bit unique ID
type NetworkID uint64

// NewNetworkIDFromString parses a network ID in string form
func NewNetworkIDFromString(s string) (NetworkID, error) {
	if len(s) != 16 {
		return NetworkID(0), ErrInvalidNetworkID
	}
	n, err := strconv.ParseUint(s, 16, 64)
	return NetworkID(n), err
}

// NewNetworkIDFromBytes reads an 8-byte / 64-bit network ID.
func NewNetworkIDFromBytes(b []byte) (NetworkID, error) {
	if len(b) < 8 {
		return NetworkID(0), ErrInvalidNetworkID
	}
	return NetworkID(binary.BigEndian.Uint64(b)), nil
}

// Controller gets the Address of this network's controller.
func (n NetworkID) Controller() Address {
	return Address(uint64(n) >> 24)
}

// String returns this network ID's 16-digit hex identifier
func (n NetworkID) String() string {
	return fmt.Sprintf("%.16x", uint64(n))
}

// Bytes returns this network ID as an 8-byte / 64-bit big-endian value.
func (n NetworkID) Bytes() []byte {
	var b [8]byte
	binary.BigEndian.PutUint64(b[:], uint64(n))
	return b[:]
}

// MarshalJSON marshals this NetworkID as a string
func (n NetworkID) MarshalJSON() ([]byte, error) {
	return []byte("\"" + n.String() + "\""), nil
}

// UnmarshalJSON unmarshals this NetworkID from a string
func (n *NetworkID) UnmarshalJSON(j []byte) error {
	var s string
	err := json.Unmarshal(j, &s)
	if err != nil {
		return err
	}
	*n, err = NewNetworkIDFromString(s)
	return err
}

// NetworkConfig represents the network's current configuration as distributed by its network controller.
type NetworkConfig struct {
	// ID is this network's 64-bit globally unique identifier
	ID NetworkID `json:"id"`

	// MAC is the Ethernet MAC address of this device on this network
	MAC MAC `json:"mac"`

	// Name is a short human-readable name set by the controller
	Name string `json:"name"`

	// Status is a status code indicating this network's authorization status
	Status int `json:"status"`

	// Type is this network's type
	Type int `json:"type"`

	// MTU is the Ethernet MTU for this network
	MTU int `json:"mtu"`

	// Bridge is true if this network is allowed to bridge in other devices with different Ethernet addresses
	Bridge bool `json:"bridge"`

	// BroadcastEnabled is true if the broadcast (ff:ff:ff:ff:ff:ff) address works (excluding IPv4 ARP which is handled via a special path)
	BroadcastEnabled bool `json:"broadcastEnabled"`

	// NetconfRevision is the revision number reported by the controller
	NetconfRevision uint64 `json:"netconfRevision"`

	// AssignedAddresses are static IPs assigned by the network controller to this device
	AssignedAddresses []InetAddress `json:"assignedAddresses,omitempty"`

	// Routes are static routes assigned by the network controller to this device
	Routes []Route `json:"routes,omitempty"`
}

// NetworkLocalSettings is settings for this network that can be changed locally
type NetworkLocalSettings struct {
	// AllowManagedIPs determines whether managed IP assignment is allowed
	AllowManagedIPs bool `json:"allowManagedIPs"`

	// AllowGlobalIPs determines if managed IPs that overlap with public Internet addresses are allowed
	AllowGlobalIPs bool `json:"allowGlobalIPs"`

	// AllowManagedRoutes determines whether managed routes can be set
	AllowManagedRoutes bool `json:"allowManagedRoutes"`

	// AllowGlobalRoutes determines if managed routes can overlap with public Internet addresses
	AllowGlobalRoutes bool `json:"allowGlobalRoutes"`

	// AllowDefaultRouteOverride determines if the default (0.0.0.0 or ::0) route on the system can be overridden ("full tunnel" mode)
	AllowDefaultRouteOverride bool `json:"allowDefaultRouteOverride"`
}

// Network is a currently joined network
type Network struct {
	node                       *Node
	id                         NetworkID
	mac                        MAC
	tap                        Tap
	config                     NetworkConfig
	settings                   NetworkLocalSettings // locked by configLock
	multicastSubscriptions     map[[2]uint64]*MulticastGroup
	configLock                 sync.RWMutex
	multicastSubscriptionsLock sync.RWMutex
}

// newNetwork creates a new network with default settings
func newNetwork(node *Node, id NetworkID, t Tap) (*Network, error) {
	m := NewMACForNetworkMember(node.Identity().address, id)
	n := &Network{
		node: node,
		id:   id,
		mac:  m,
		tap:  t,
		config: NetworkConfig{
			ID:     id,
			MAC:    m,
			Status: NetworkStatusRequestingConfiguration,
			Type:   NetworkTypePrivate,
			MTU:    int(defaultVirtualNetworkMTU),
		},
		settings: NetworkLocalSettings{
			AllowManagedIPs:           true,
			AllowGlobalIPs:            false,
			AllowManagedRoutes:        true,
			AllowGlobalRoutes:         false,
			AllowDefaultRouteOverride: false,
		},
		multicastSubscriptions: make(map[[2]uint64]*MulticastGroup),
	}

	t.AddMulticastGroupChangeHandler(func(added bool, mg *MulticastGroup) {
		if added {
			n.MulticastSubscribe(mg)
		} else {
			n.MulticastUnsubscribe(mg)
		}
	})

	return n, nil
}

// ID gets this network's unique ID
func (n *Network) ID() NetworkID { return n.id }

// MAC returns the assigned MAC address of this network
func (n *Network) MAC() MAC { return n.mac }

// Tap gets this network's tap device
func (n *Network) Tap() Tap { return n.tap }

// Config returns a copy of this network's current configuration
func (n *Network) Config() NetworkConfig {
	n.configLock.RLock()
	defer n.configLock.RUnlock()
	return n.config
}

// SetLocalSettings modifies this network's local settings
func (n *Network) SetLocalSettings(ls *NetworkLocalSettings) { n.updateConfig(nil, ls) }

// LocalSettings gets this network's current local settings
func (n *Network) LocalSettings() NetworkLocalSettings {
	n.configLock.RLock()
	defer n.configLock.RUnlock()
	return n.settings
}

// MulticastSubscribe subscribes to a multicast group
func (n *Network) MulticastSubscribe(mg *MulticastGroup) {
	n.node.infoLog.Printf("%.16x joined multicast group %s", uint64(n.id), mg.String())
	k := mg.key()
	n.multicastSubscriptionsLock.Lock()
	if _, have := n.multicastSubscriptions[k]; have {
		n.multicastSubscriptionsLock.Unlock()
		return
	}
	n.multicastSubscriptions[k] = mg
	n.multicastSubscriptionsLock.Unlock()
	n.node.multicastSubscribe(uint64(n.id), mg)
}

// MulticastUnsubscribe removes a subscription to a multicast group
func (n *Network) MulticastUnsubscribe(mg *MulticastGroup) {
	n.node.infoLog.Printf("%.16x left multicast group %s", uint64(n.id), mg.String())
	n.multicastSubscriptionsLock.Lock()
	delete(n.multicastSubscriptions, mg.key())
	n.multicastSubscriptionsLock.Unlock()
	n.node.multicastUnsubscribe(uint64(n.id), mg)
}

// MulticastSubscriptions returns an array of all multicast subscriptions for this network
func (n *Network) MulticastSubscriptions() []*MulticastGroup {
	n.multicastSubscriptionsLock.RLock()
	mgs := make([]*MulticastGroup, 0, len(n.multicastSubscriptions))
	for _, mg := range n.multicastSubscriptions {
		mgs = append(mgs, mg)
	}
	n.multicastSubscriptionsLock.RUnlock()
	sort.Slice(mgs, func(a, b int) bool { return mgs[a].Less(mgs[b]) })
	return mgs
}

// leaving is called by Node when the network is being left
func (n *Network) leaving() {
	n.tap.Close()
}

func (n *Network) networkConfigRevision() uint64 {
	n.configLock.RLock()
	defer n.configLock.RUnlock()
	return n.config.NetconfRevision
}

func networkManagedIPAllowed(ip net.IP, ls *NetworkLocalSettings) bool {
	if !ls.AllowManagedIPs {
		return false
	}
	switch ipClassify(ip) {
	case ipClassificationNone, ipClassificationLoopback, ipClassificationLinkLocal, ipClassificationMulticast:
		return false
	case ipClassificationGlobal:
		return ls.AllowGlobalIPs
	}
	return true
}

func networkManagedRouteAllowed(r *Route, ls *NetworkLocalSettings) bool {
	if !ls.AllowManagedRoutes {
		return false
	}
	bits, _ := r.Target.Mask.Size()
	if len(r.Target.IP) > 0 && allZero(r.Target.IP) && bits == 0 {
		return ls.AllowDefaultRouteOverride
	}
	switch ipClassify(r.Target.IP) {
	case ipClassificationNone, ipClassificationLoopback, ipClassificationLinkLocal, ipClassificationMulticast:
		return false
	case ipClassificationGlobal:
		return ls.AllowGlobalRoutes
	}
	return true
}

func (n *Network) updateConfig(nc *NetworkConfig, ls *NetworkLocalSettings) {
	n.configLock.Lock()
	defer n.configLock.Unlock()

	if n.tap == nil { // sanity check, should never happen
		return
	}

	if nc == nil {
		nc = &n.config
	}
	if ls == nil {
		ls = &n.settings
	}

	// Add IPs to tap that are newly assigned in this config update,
	// and remove any IPs from the tap that were assigned that are no
	// longer wanted. IPs assigned to the tap externally (e.g. by an
	// "ifconfig" command) are left alone.
	haveAssignedIPs := make(map[[3]uint64]*InetAddress)
	wantAssignedIPs := make(map[[3]uint64]bool)
	if n.settings.AllowManagedIPs {
		for _, ip := range n.config.AssignedAddresses {
			if networkManagedIPAllowed(ip.IP, &n.settings) { // was it allowed?
				haveAssignedIPs[ipNetToKey(&ip)] = &ip
			}
		}
	}
	if ls.AllowManagedIPs {
		for _, ip := range nc.AssignedAddresses {
			if networkManagedIPAllowed(ip.IP, ls) { // should it be allowed now?
				k := ipNetToKey(&ip)
				wantAssignedIPs[k] = true
				if _, have := haveAssignedIPs[k]; !have {
					n.node.infoLog.Printf("%.16x adding managed IP %s", uint64(n.id), ip.String())
					_ = n.tap.AddIP(&ip)
				}
			}
		}
	}
	for k, ip := range haveAssignedIPs {
		if _, want := wantAssignedIPs[k]; !want {
			n.node.infoLog.Printf("%.16x removing managed IP %s", uint64(n.id), ip.String())
			_ = n.tap.RemoveIP(ip)
		}
	}

	// Do the same for managed routes
	haveManagedRoutes := make(map[[6]uint64]*Route)
	wantManagedRoutes := make(map[[6]uint64]bool)
	if n.settings.AllowManagedRoutes {
		for _, r := range n.config.Routes {
			if networkManagedRouteAllowed(&r, &n.settings) { // was it allowed?
				haveManagedRoutes[r.key()] = &r
			}
		}
	}
	if ls.AllowManagedRoutes {
		for _, r := range nc.Routes {
			if networkManagedRouteAllowed(&r, ls) { // should it be allowed now?
				k := r.key()
				wantManagedRoutes[k] = true
				if _, have := haveManagedRoutes[k]; !have {
					n.node.infoLog.Printf("%.16x adding managed route %s", uint64(n.id), r.String())
					//TODO _ = n.tap.AddRoute(&r)
				}
			}
		}
	}
	for k, r := range haveManagedRoutes {
		if _, want := wantManagedRoutes[k]; !want {
			n.node.infoLog.Printf("%.16x removing managed route %s", uint64(n.id), r.String())
			//TODO _ = n.tap.RemoveRoute(r)
		}
	}

	if nc != &n.config {
		n.config = *nc
	}
	if ls != &n.settings {
		n.settings = *ls
	}
}
