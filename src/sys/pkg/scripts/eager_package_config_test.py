#!/usr/bin/env python3.8
# Copyright 2022 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import unittest

from eager_package_config import generate_omaha_client_config


class TestEagerPackageConfig(unittest.TestCase):
    maxDiff = None

    def test_generate_omaha_client_config(self):
        configs = [
            {
                "url":
                    "fuchsia-pkg://example.com/package",
                "default_channel":
                    "stable",
                "flavor":
                    "debug",
                "realms":
                    [
                        {
                            "app_id": "1a2b3c4d",
                            "channels": ["stable", "beta", "alpha"],
                        },
                        {
                            "app_id": "2b3c4d5e",
                            "channels": ["test"],
                        },
                    ],
            }, {
                "url": "fuchsia-pkg://example.com/package2",
                "realms": [{
                    "app_id": "3c4d5e6f",
                    "channels": ["stable"],
                }],
            }
        ]

        self.assertEqual(
            generate_omaha_client_config(configs), {
                "packages":
                    [
                        {
                            "url": "fuchsia-pkg://example.com/package",
                            "flavor": "debug",
                            "channel_config":
                                {
                                    "channels":
                                        [
                                            {
                                                "name": "stable",
                                                "repo": "stable",
                                                "appid": "1a2b3c4d",
                                            },
                                            {
                                                "name": "beta",
                                                "repo": "beta",
                                                "appid": "1a2b3c4d",
                                            },
                                            {
                                                "name": "alpha",
                                                "repo": "alpha",
                                                "appid": "1a2b3c4d",
                                            },
                                            {
                                                "name": "test",
                                                "repo": "test",
                                                "appid": "2b3c4d5e",
                                            },
                                        ],
                                    "default_channel": "stable",
                                }
                        },
                        {
                            "url": "fuchsia-pkg://example.com/package2",
                            "channel_config":
                                {
                                    "channels":
                                        [
                                            {
                                                "name": "stable",
                                                "repo": "stable",
                                                "appid": "3c4d5e6f",
                                            }
                                        ],
                                }
                        },
                    ]
            })

    def test_generate_omaha_client_config_wrong_default_channel(self):
        configs = [
            {
                "url":
                    "fuchsia-pkg://example.com/package",
                "default_channel":
                    "wrong",
                "realms":
                    [
                        {
                            "app_id": "1a2b3c4d",
                            "channels": ["stable", "beta", "alpha"]
                        }
                    ]
            }
        ]
        with self.assertRaises(AssertionError):
            generate_omaha_client_config(configs)
