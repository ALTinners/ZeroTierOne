/*
 * Copyright (c)2013-2021 ZeroTier, Inc.
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

package cli

import (
	"encoding/hex"
	"fmt"
	"io/ioutil"
	"strings"

	"zerotier/pkg/zerotier"
)

func Identity(args []string) int {
	if len(args) > 0 {
		switch args[0] {

		case "new":
			idType := zerotier.IdentityTypeC25519
			if len(args) > 1 {
				if len(args) > 2 {
					Help()
					return 1
				}
				switch args[1] {
				case "c25519", "C25519", "0":
					idType = zerotier.IdentityTypeC25519
				case "p384", "P384", "1":
					idType = zerotier.IdentityTypeP384
				default:
					Help()
					return 1
				}
			}

			id, err := zerotier.NewIdentity(idType)
			if err != nil {
				pErr("internal error generating identity: %s", err.Error())
				return 1
			}

			fmt.Println(id.PrivateKeyString())
			return 0

		case "getpublic":
			if len(args) == 2 {
				fmt.Println(cliGetIdentityOrFatal(args[1]).String())
				return 0
			}
			pErr("no identity specified")
			return 1

		case "fingerprint":
			if len(args) == 2 {
				fmt.Println(cliGetIdentityOrFatal(args[1]).Fingerprint().String())
				return 0
			}
			pErr("no identity specified")
			return 1

		case "validate":
			if len(args) == 2 {
				if cliGetIdentityOrFatal(args[1]).LocallyValidate() {
					fmt.Println("VALID")
					return 0
				}
				fmt.Println("INVALID")
				return 1
			}

		case "sign", "verify":
			if len(args) > 2 {
				id := cliGetIdentityOrFatal(args[1])
				msg, err := ioutil.ReadFile(args[2])
				if err != nil {
					pErr("unable to read input file: %s", err.Error())
					return 1
				}

				if args[0] == "verify" {
					if len(args) == 4 {
						sig, err := hex.DecodeString(strings.TrimSpace(args[3]))
						if err != nil {
							fmt.Println("FAILED")
							return 1
						}
						if id.Verify(msg, sig) {
							fmt.Println("OK")
							return 0
						}
					}
					fmt.Println("FAILED")
					return 1
				} else {
					sig, err := id.Sign(msg)
					if err != nil {
						pErr("internal error signing message: %s", err.Error())
						return 1
					}
					fmt.Println(hex.EncodeToString(sig))
					return 0
				}
			}

		}
	}

	Help()
	return 1
}
