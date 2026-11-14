# Node refusal stubs: safe delete list

Recorded from noevia-core `2ac77007` (pairing lines from noevia `11bf2da7`) by `tools/gen-node-refusal-stubs.cjs`; verdicts from the front's own dispatch with every switch off (`crates/route-pins`). 68 stubs: 48 files can go (1300 lines), 18 refusal functions can go from files that stay, 2 stay.

A deletion removes the file (or function), its `index.cjs` call, the core tests that only test the refusal, and changes the listed supervisor/assembler pairing lines in the same train (they read the stub's marker or route set). Delete nothing whose verdict is keep.

| Stub | Function | Lines | Refusals | Verdict | Pairing lines / why |
|---|---|---:|---:|---|---|
| `server/rust-auto-roles-write.cjs` | `refuseOwned` | 35 | 1 | delete function | imported by server/index.cjs (createBridge), server/routes/models.cjs; pairing: build/web-supervisor.sh:351, deploy/tools/assemble-release.sh:2683, deploy/tools/assemble-release.sh:2688 |
| `server/rust-tool-approvals.cjs` | `refuseOwned` | 14 | 1 | delete file | pairing: build/web-supervisor.sh:309, deploy/tools/assemble-release.sh:1696, deploy/tools/assemble-release.sh:1714, deploy/tools/assemble-release.sh:1730 |
| `server/rust-model-writes.cjs` | `refuseOwned` | 35 | 0 | keep (switch) | reads a deployment switch |
| `server/rust-idle-unload-read.cjs` | `refuseOwned` | 9 | 1 | delete file | pairing: build/web-supervisor.sh:297, deploy/tools/assemble-release.sh:2065, deploy/tools/assemble-release.sh:2079 |
| `server/rust-model-downloads-read.cjs` | `refuseOwned` | 9 | 1 | delete file | pairing: build/web-supervisor.sh:299, deploy/tools/assemble-release.sh:2169, deploy/tools/assemble-release.sh:2183 |
| `server/rust-autotune-read.cjs` | `refuseOwned` | 9 | 1 | delete file | pairing: build/web-supervisor.sh:296, deploy/tools/assemble-release.sh:1828, deploy/tools/assemble-release.sh:1872 |
| `server/rust-preset-read.cjs` | `refuseOwned` | 9 | 1 | delete file | pairing: build/web-supervisor.sh:300, deploy/tools/assemble-release.sh:2200, deploy/tools/assemble-release.sh:2214 |
| `server/rust-autotune-resume.cjs` | `refuseOwned` | 9 | 7 | delete file | pairing: build/web-supervisor.sh:301, deploy/tools/assemble-release.sh:2269, deploy/tools/assemble-release.sh:2283 |
| `server/rust-autotune-clear.cjs` | `refuseOwned` | 11 | 7 | delete file | pairing: build/web-supervisor.sh:303, deploy/tools/assemble-release.sh:2234, deploy/tools/assemble-release.sh:2248 |
| `server/rust-autotune-untuned.cjs` | `refuseOwned` | 10 | 7 | delete file | pairing: build/web-supervisor.sh:306, deploy/tools/assemble-release.sh:2365, deploy/tools/assemble-release.sh:2379 |
| `server/features.cjs` | `refuseRustStorageRead` | 216 | 1 | delete function | imported by server/index.cjs (createReadOnlyFeatures, settingsStore), server/routes/storage.cjs, tools/features/check-catalog-mirror.cjs, tools/gen-model-manager-routes.cjs; pairing: none |
| `server/routes/chat-framing.cjs` | `refuseOwnedPreferences` | 50 | 7 | delete function | imported by server/index.cjs (createChatFramingRoutes), server/chat-frame-steering.cjs, server/chat-lists.cjs, tools/decision-eval/build-requests.cjs; pairing: deploy/tools/assemble-release.sh:1755, deploy/tools/assemble-release.sh:1777 |
| `server/rust-chat-framing-suggest.cjs` | `refuseOwned` | 88 | 1 | delete function | imported by server/index.cjs (createSuggestBridge), server/routes/chat-framing.cjs; pairing: build/web-supervisor.sh:312, deploy/tools/assemble-release.sh:1754, deploy/tools/assemble-release.sh:1758, deploy/tools/assemble-release.sh:1775 |
| `server/rust-chat-turn.cjs` | `refuseOwned` | 99 | 1 | delete function | imported by server/index.cjs (createTurnBridge, owned, relayedUser, relayedWire), server/chat.cjs, server/rust-chat-native.cjs; pairing: deploy/tools/assemble-release.sh:940, deploy/tools/assemble-release.sh:945, deploy/tools/assemble-release.sh:946, deploy/tools/assemble-release.sh:947 |
| `server/routes/features.cjs` | `refuseOwnedFeatures` | 21 | 35 | delete function | imported by server/index.cjs (createFeatureRoutes), server/routes/storage.cjs, tools/features/check-catalog-mirror.cjs, tools/gen-model-manager-routes.cjs; pairing: none |
| `server/rust-diary.cjs` | `refuseOwned` | 50 | 72 | delete function | imported by server/index.cjs (owned); pairing: build/web-supervisor.sh:101, build/web-supervisor.sh:105, build/web-supervisor.sh:493, deploy/tools/assemble-release.sh:859, deploy/tools/assemble-release.sh:863, deploy/tools/assemble-release.sh:1608 |
| `server/rust-diary-routes.cjs` | `refuseOwned` | 65 | 27 | delete function | imported by server/index.cjs (owned), server/rust-auth.cjs; pairing: build/web-supervisor.sh:244, build/web-supervisor.sh:286, deploy/tools/assemble-release.sh:981, deploy/tools/assemble-release.sh:988, deploy/tools/assemble-release.sh:1019, deploy/tools/assemble-release.sh:1022 |
| `server/rust-diary-local-exchange.cjs` | `refuseOwned` | 33 | 7 | delete function | imported by server/index.cjs (owned); pairing: build/web-supervisor.sh:246, build/web-supervisor.sh:287, deploy/tools/assemble-release.sh:898, deploy/tools/assemble-release.sh:905 |
| `server/rust-models.cjs` | `refuseOwned` | 37 | 2 | delete file | pairing: build/web-supervisor.sh:112, build/web-supervisor.sh:117, deploy/tools/assemble-release.sh:1026, deploy/tools/assemble-release.sh:1031; comments in: server/routes/models.test.cjs, server/rust-model-sizing.cjs, server/rust-model-state-reads.cjs |
| `server/rust-model-state-reads.cjs` | `refuseOwned` | 26 | 2 | delete file | pairing: build/web-supervisor.sh:255, build/web-supervisor.sh:311, deploy/tools/assemble-release.sh:3203, deploy/tools/assemble-release.sh:3209, deploy/tools/assemble-release.sh:3231; comments in: server/routes/models.cjs, server/rust-models.cjs |
| `server/rust-autotune-settings.cjs` | `refuseOwned` | 11 | 7 | delete file | pairing: build/web-supervisor.sh:308, deploy/tools/assemble-release.sh:1660, deploy/tools/assemble-release.sh:1674 |
| `server/rust-project-reads.cjs` | `refuseOwned` | 52 | 2 | delete file | pairing: build/web-supervisor.sh:124, build/web-supervisor.sh:129, deploy/tools/assemble-release.sh:1051, deploy/tools/assemble-release.sh:1056, deploy/tools/assemble-release.sh:4128, deploy/tools/assemble-release.sh:4135; comments in: tests/server/stored-sources.cjs |
| `server/rust-project-lifecycle.cjs` | `refuseOwned` | 49 | 2 | delete file | pairing: build/web-supervisor.sh:136, build/web-supervisor.sh:142, deploy/tools/assemble-release.sh:1076, deploy/tools/assemble-release.sh:1081, deploy/tools/assemble-release.sh:4129, deploy/tools/assemble-release.sh:4135; comments in: server/rag.cjs, tests/server/front-projects.cjs |
| `server/rust-project-files.cjs` | `refuseOwned` | 37 | 1 | delete file | pairing: build/web-supervisor.sh:173, build/web-supervisor.sh:177, deploy/tools/assemble-release.sh:1127, deploy/tools/assemble-release.sh:1132, deploy/tools/assemble-release.sh:4130, deploy/tools/assemble-release.sh:4135; comments in: server/rust-project-storage.cjs |
| `server/rust-project-document-pages.cjs` | `refuseOwned` | 36 | 1 | delete file | pairing: build/web-supervisor.sh:184, build/web-supervisor.sh:188, deploy/tools/assemble-release.sh:2433, deploy/tools/assemble-release.sh:2438, deploy/tools/assemble-release.sh:4131, deploy/tools/assemble-release.sh:4135; comments in: server/rust-project-reads.cjs, tests/server/stored-sources.cjs |
| `server/rust-conversation-import.cjs` | `refuseOwned` | 35 | 1 | delete file | pairing: build/web-supervisor.sh:195, build/web-supervisor.sh:199, deploy/tools/assemble-release.sh:575, deploy/tools/assemble-release.sh:576, deploy/tools/assemble-release.sh:2457, deploy/tools/assemble-release.sh:2462; comments in: server/routes/import.cjs, server/rust-project-lifecycle.cjs |
| `server/rust-project-documents.cjs` | `refuseOwned` | 43 | 1 | delete file | pairing: build/web-supervisor.sh:206, build/web-supervisor.sh:210, deploy/tools/assemble-release.sh:2482, deploy/tools/assemble-release.sh:2487, deploy/tools/assemble-release.sh:4132, deploy/tools/assemble-release.sh:4135 |
| `server/rust-source-jobs.cjs` | `refuseOwned` | 36 | 1 | delete file | pairing: build/web-supervisor.sh:553, build/web-supervisor.sh:558, deploy/tools/assemble-release.sh:3401, deploy/tools/assemble-release.sh:3408, deploy/tools/assemble-release.sh:4132, deploy/tools/assemble-release.sh:4135; comments in: server/rust-project-source-work.cjs |
| `server/rust-project-source-work.cjs` | `refuseOwned` | 46 | 3 | delete file | pairing: build/web-supervisor.sh:566, build/web-supervisor.sh:570, deploy/tools/assemble-release.sh:3350, deploy/tools/assemble-release.sh:3358, deploy/tools/assemble-release.sh:4133, deploy/tools/assemble-release.sh:4135; comments in: server/rust-project-documents.test.cjs, server/rust-source-jobs.cjs, server/rust-source-jobs.test.cjs |
| `server/rust-research.cjs` | `refuseOwned` | 39 | 14 | delete file | pairing: build/web-supervisor.sh:254, build/web-supervisor.sh:294, deploy/tools/assemble-release.sh:1198, deploy/tools/assemble-release.sh:1204, deploy/tools/assemble-release.sh:1221, deploy/tools/assemble-release.sh:4134, deploy/tools/assemble-release.sh:4135 |
| `server/rust-task-reads.cjs` | `refuseOwned` | 33 | 9 | delete file | pairing: build/web-supervisor.sh:255, build/web-supervisor.sh:295, deploy/tools/assemble-release.sh:1256, deploy/tools/assemble-release.sh:1263, deploy/tools/assemble-release.sh:1290, deploy/tools/assemble-release.sh:4134, deploy/tools/assemble-release.sh:4135; comments in: server/routes/browser.cjs, server/routes/code.cjs |
| `server/rust-project-config.cjs` | `refuseOwned` | 59 | 6 | delete file | pairing: build/web-supervisor.sh:252, build/web-supervisor.sh:292, deploy/tools/assemble-release.sh:1152, deploy/tools/assemble-release.sh:1160, deploy/tools/assemble-release.sh:1189, deploy/tools/assemble-release.sh:4134, deploy/tools/assemble-release.sh:4135; comments in: server/instruction-skills.cjs |
| `server/rust-admin-users.cjs` | `refuseOwned` | 44 | 1 | delete file | pairing: build/web-supervisor.sh:253, build/web-supervisor.sh:293, deploy/tools/assemble-release.sh:2402, deploy/tools/assemble-release.sh:2408, deploy/tools/assemble-release.sh:2425; comments in: server/auth.cjs, server/routes/auth.cjs, server/routes/auth.test.cjs, server/rust-auth.cjs, server/rust-auth.test.cjs, server/workspace.cjs |
| `server/rust-model-sizing.cjs` | `refuseOwned` | 34 | 2 | delete file | pairing: build/web-supervisor.sh:365, build/web-supervisor.sh:370, deploy/tools/assemble-release.sh:3178, deploy/tools/assemble-release.sh:3183; comments in: server/rust-models.cjs |
| `server/rust-model-maintenance.cjs` | `refuseOwned` | 61 | 3 | delete file | pairing: build/web-supervisor.sh:379, build/web-supervisor.sh:385, deploy/tools/assemble-release.sh:3250, deploy/tools/assemble-release.sh:3257; comments in: server/routes/models.cjs, server/routes/models.test.cjs |
| `server/rust-model-evidence-recheck.cjs` | `refuseOwned` | 13 | 1 | delete file | pairing: build/web-supervisor.sh:310, deploy/tools/assemble-release.sh:3285, deploy/tools/assemble-release.sh:3304; comments in: server/rust-model-maintenance.cjs |
| `server/rust-public-pages.cjs` | `refuseOwned` | 30 | 18 | delete file | pairing: build/web-supervisor.sh:216, build/web-supervisor.sh:267, deploy/tools/assemble-release.sh:2507, deploy/tools/assemble-release.sh:2512; comments in: server/routes/device-auth.cjs, server/rust-auth.test.cjs |
| `server/rust-plugin-directory.cjs` | `refuseOwned` | 25 | 7 | delete file | pairing: build/web-supervisor.sh:217, build/web-supervisor.sh:268, deploy/tools/assemble-release.sh:2535, deploy/tools/assemble-release.sh:2539 |
| `server/rust-decision-settings.cjs` | `refuseOwned` | 25 | 14 | delete file | pairing: build/web-supervisor.sh:218, build/web-supervisor.sh:269, deploy/tools/assemble-release.sh:2746, deploy/tools/assemble-release.sh:2751; comments in: server/routes/features.cjs |
| `server/rust-storage-integrations.cjs` | `refuseOwned` | 54 | 5 | delete function | imported by server/index.cjs (createProofReader), server/routes/storage.cjs; pairing: build/web-supervisor.sh:220, build/web-supervisor.sh:270, deploy/tools/assemble-release.sh:2767, deploy/tools/assemble-release.sh:2773 |
| `server/rust-storage-connection.cjs` | `refuseOwned` | 39 | 1 | delete function | imported by server/auth.cjs, server/routes/storage.cjs, server/rust-auth.cjs; pairing: build/web-supervisor.sh:223, build/web-supervisor.sh:272, deploy/tools/assemble-release.sh:2815, deploy/tools/assemble-release.sh:2821 |
| `server/rust-provider-test.cjs` | `refuseOwned` | 21 | 1 | delete file | pairing: build/web-supervisor.sh:225, build/web-supervisor.sh:273, deploy/tools/assemble-release.sh:2897, deploy/tools/assemble-release.sh:2904 |
| `server/rust-chatgpt-oauth.cjs` | `refuseOwned` | 44 | 4 | delete function | imported by server/index.cjs (SIGNAL_FILE), server/rust-auth.cjs; pairing: build/web-supervisor.sh:226, build/web-supervisor.sh:274, deploy/tools/assemble-release.sh:2926, deploy/tools/assemble-release.sh:2931 |
| `server/rust-provider-models.cjs` | `refuseOwned` | 28 | 7 | delete file | pairing: build/web-supervisor.sh:229, build/web-supervisor.sh:275, deploy/tools/assemble-release.sh:3098, deploy/tools/assemble-release.sh:3103 |
| `server/rust-providers.cjs` | `refuseOwned` | 36 | 7 | delete file | pairing: build/web-supervisor.sh:230, build/web-supervisor.sh:276, deploy/tools/assemble-release.sh:2840, deploy/tools/assemble-release.sh:2845; comments in: server/providers.cjs, server/workspace.cjs |
| `server/rust-residual-fallbacks.cjs` | `refuseOwned` | 35 | 332 | delete file | pairing: deploy/tools/assemble-release.sh:2137, deploy/tools/assemble-release.sh:2142, deploy/tools/assemble-release.sh:2146, deploy/tools/assemble-release.sh:2166, deploy/tools/assemble-release.sh:4405 |
| `server/rust-mcp-credentials.cjs` | `refuseOwned` | 222 | 9 | delete function | imported by server/index.cjs (createMcpCredentials, ensureSchema, resealMcpCredentials), server/rust-auth.cjs, server/secrets-rotate.cjs; pairing: build/web-supervisor.sh:238, build/web-supervisor.sh:280, deploy/tools/assemble-release.sh:2957, deploy/tools/assemble-release.sh:3043 |
| `server/rust-mcp-directory.cjs` | `refuseOwned` | 43 | 56 | delete file | pairing: build/web-supervisor.sh:248, build/web-supervisor.sh:288, deploy/tools/assemble-release.sh:3003; comments in: server/directory-mcp.cjs, server/rust-mcp-credentials.cjs |
| `server/rust-toolboxes-permitted.cjs` | `refuseOwned` | 27 | 7 | delete file | pairing: build/web-supervisor.sh:249, build/web-supervisor.sh:290, deploy/tools/assemble-release.sh:3003; comments in: server/toolboxes-permitted.cjs |
| `server/rust-toolbox-picker.cjs` | `refuseOwned` | 12 | 7 | delete file | pairing: build/web-supervisor.sh:289, deploy/tools/assemble-release.sh:1790, deploy/tools/assemble-release.sh:1804 |
| `server/rust-model-assignments.cjs` | `refuseOwned` | 9 | 1 | delete file | pairing: build/web-supervisor.sh:298, deploy/tools/assemble-release.sh:2099, deploy/tools/assemble-release.sh:2122 |
| `server/rust-calibration-routes.cjs` | `refuseOwned` | 15 | 7 | delete file | pairing: build/web-supervisor.sh:307, build/web-supervisor.sh:521, deploy/tools/assemble-release.sh:1414, deploy/tools/assemble-release.sh:1428, deploy/tools/assemble-release.sh:1633, deploy/tools/assemble-release.sh:1643, deploy/tools/assemble-release.sh:1644 |
| `server/rust-model-manager-proxy.cjs` | `refuseOwned` | 41 | 0 | keep (switch) | reads a deployment switch |
| `server/rust-model-manager-scan.cjs` | `refuseOwned` | 10 | 1 | delete file | pairing: build/web-supervisor.sh:304, deploy/tools/assemble-release.sh:1314, deploy/tools/assemble-release.sh:1344; comments in: server/rust-model-manager-proxy.cjs |
| `server/rust-autotune-cancel.cjs` | `refuseOwned` | 11 | 7 | delete file | pairing: build/web-supervisor.sh:302, deploy/tools/assemble-release.sh:2330, deploy/tools/assemble-release.sh:2344 |
| `server/rust-calibration-cancel.cjs` | `refuseOwned` | 11 | 7 | delete file | pairing: build/web-supervisor.sh:305, deploy/tools/assemble-release.sh:1379, deploy/tools/assemble-release.sh:1393 |
| `server/rust-connectors.cjs` | `refuseOwned` | 30 | 2 | delete file | pairing: build/web-supervisor.sh:250, build/web-supervisor.sh:291, deploy/tools/assemble-release.sh:3003; comments in: server/rust-google-drive.cjs, server/tool-policy.cjs |
| `server/rust-vault-mirror-preferences.cjs` | `refuseOwned` | 22 | 7 | delete file | pairing: build/web-supervisor.sh:221, build/web-supervisor.sh:271, deploy/tools/assemble-release.sh:2794, deploy/tools/assemble-release.sh:2799 |
| `server/rust-google-drive.cjs` | `refuseOwned` | 42 | 4 | delete function | imported by server/gdrive-broker-fixture.cjs, server/gdrive-broker.cjs, server/rust-auth.cjs; pairing: build/web-supervisor.sh:235, build/web-supervisor.sh:278, deploy/tools/assemble-release.sh:3119, deploy/tools/assemble-release.sh:3124 |
| `server/rust-offsite-backup.cjs` | `refuseOwned` | 31 | 56 | delete file | pairing: build/web-supervisor.sh:237, build/web-supervisor.sh:279, deploy/tools/assemble-release.sh:3150, deploy/tools/assemble-release.sh:3155, deploy/tools/assemble-release.sh:3173 |
| `server/rust-health.cjs` | `refuseOwned` | 66 | 1 | delete function | imported by server/index.cjs (createRagStatusPublisher); pairing: build/web-supervisor.sh:241, build/web-supervisor.sh:281, deploy/tools/assemble-release.sh:2696, deploy/tools/assemble-release.sh:2726 |
| `server/rust-auto-roles.cjs` | `refuseOwned` | 75 | 1 | delete function | imported by server/index.cjs (createAutoRolesPublisher); pairing: build/web-supervisor.sh:242, build/web-supervisor.sh:282, deploy/tools/assemble-release.sh:2696, deploy/tools/assemble-release.sh:2727 |
| `server/rust-workspace.cjs` | `refuseOwned` | 24 | 1 | delete file | pairing: build/web-supervisor.sh:243, build/web-supervisor.sh:283, deploy/tools/assemble-release.sh:2697, deploy/tools/assemble-release.sh:2728; comments in: server/projects.cjs, server/routes/chat-lists.cjs, tests/oracles/media-u2/projects.cjs |
| `server/rust-chat-search.cjs` | `refuseOwned` | 26 | 1 | delete file | pairing: build/web-supervisor.sh:284, deploy/tools/assemble-release.sh:2730, deploy/tools/assemble-release.sh:2732, deploy/tools/assemble-release.sh:2734, deploy/tools/assemble-release.sh:2735; comments in: server/auth.test.cjs, server/device-auth-routes.test.cjs, server/routes/chat-lists.cjs, tools/contract-corpus/generate.cjs |
| `server/rust-chat-export.cjs` | `refuseOwned` | 27 | 1 | delete file | pairing: build/web-supervisor.sh:285, deploy/tools/assemble-release.sh:2730, deploy/tools/assemble-release.sh:2733, deploy/tools/assemble-release.sh:2740, deploy/tools/assemble-release.sh:2741; comments in: server/device-auth-routes.test.cjs, server/routes/chat-lists.cjs |
| `server/rust-auth.cjs` | `refuseOwned` | 325 | 105 | delete function | imported by server/index.cjs (createRustAuthGuard), server/auth.cjs; pairing: build/web-supervisor.sh:73, deploy/tools/assemble-release.sh:528, deploy/tools/assemble-release.sh:1022, deploy/tools/assemble-release.sh:2305, deploy/tools/assemble-release.sh:2405, deploy/tools/assemble-release.sh:2428, deploy/tools/assemble-release.sh:2429, deploy/tools/assemble-release.sh:4139, deploy/tools/assemble-release.sh:4147, deploy/tools/assemble-release.sh:4148 |
| `server/rust-projects.cjs` | `refuseOwnedRoute` | 529 | 15 | delete function | imported by server/index.cjs (warnRetired), server/workspace.cjs; pairing: deploy/tools/assemble-release.sh:372, deploy/tools/assemble-release.sh:795, deploy/tools/assemble-release.sh:805, deploy/tools/assemble-release.sh:4120, deploy/tools/assemble-release.sh:4125, deploy/tools/assemble-release.sh:4126, deploy/tools/assemble-release.sh:4135, deploy/tools/assemble-release.sh:4136, deploy/tools/assemble-release.sh:4181, deploy/tools/assemble-release.sh:4185, deploy/tools/assemble-release.sh:4186, deploy/tools/assemble-release.sh:4196, deploy/tools/assemble-release.sh:4200, deploy/tools/assemble-release.sh:4201, deploy/tools/assemble-release.sh:4220, deploy/tools/assemble-release.sh:4222, deploy/tools/assemble-release.sh:4229 |
| `server/rust-web-address.cjs` | `refuseOwned` | 31 | 5 | delete file | pairing: build/web-supervisor.sh:477, build/web-supervisor.sh:485, deploy/tools/assemble-release.sh:518, deploy/tools/assemble-release.sh:527, deploy/tools/assemble-release.sh:533 |

## Pin candidates: refusals the front still forwards to Node (0)

Each is a request the front proxies only for Node to refuse it. A front step that answers these with the recorded bytes (`route_pins::PinTable`) makes the listed Live stubs dead. Method, routes.toml pattern, trailing slash:


## Rust-owned methods no Node stub refuses (137)

The front answers these, and nothing in core's refusal chain refuses them for a request that bypassed the front (the row's methods on the recorder's probe path). Not a leak through the front; a check for the removal: Node's own handler, if it still has one, is the code that would run.

- `GET /api/specialists`
- `HEAD /api/specialists`
- `POST /api/specialists`
- `PUT /api/specialists`
- `PATCH /api/specialists`
- `DELETE /api/specialists`
- `OPTIONS /api/specialists`
- `GET /api/specialists/{id}`
- `HEAD /api/specialists/{id}`
- `POST /api/specialists/{id}`
- `PUT /api/specialists/{id}`
- `PATCH /api/specialists/{id}`
- `DELETE /api/specialists/{id}`
- `OPTIONS /api/specialists/{id}`
- `GET /api/specialists/{id}/events`
- `HEAD /api/specialists/{id}/events`
- `POST /api/specialists/{id}/events`
- `PUT /api/specialists/{id}/events`
- `PATCH /api/specialists/{id}/events`
- `DELETE /api/specialists/{id}/events`
- `OPTIONS /api/specialists/{id}/events`
- `GET /api/spaces`
- `HEAD /api/spaces`
- `POST /api/spaces`
- `PUT /api/spaces`
- `PATCH /api/spaces`
- `DELETE /api/spaces`
- `OPTIONS /api/spaces`
- `GET /api/spaces/{id}`
- `HEAD /api/spaces/{id}`
- `POST /api/spaces/{id}`
- `PUT /api/spaces/{id}`
- `PATCH /api/spaces/{id}`
- `DELETE /api/spaces/{id}`
- `OPTIONS /api/spaces/{id}`
- `GET /api/spaces/{id}/pages`
- `HEAD /api/spaces/{id}/pages`
- `POST /api/spaces/{id}/pages`
- `PUT /api/spaces/{id}/pages`
- `PATCH /api/spaces/{id}/pages`
- `DELETE /api/spaces/{id}/pages`
- `OPTIONS /api/spaces/{id}/pages`
- `GET /api/spaces/{id}/pages/{pageId}`
- `HEAD /api/spaces/{id}/pages/{pageId}`
- `POST /api/spaces/{id}/pages/{pageId}`
- `PUT /api/spaces/{id}/pages/{pageId}`
- `PATCH /api/spaces/{id}/pages/{pageId}`
- `DELETE /api/spaces/{id}/pages/{pageId}`
- `OPTIONS /api/spaces/{id}/pages/{pageId}`
- `GET /api/chat-framing/suggest`
- `HEAD /api/chat-framing/suggest`
- `PUT /api/chat-framing/suggest`
- `PATCH /api/chat-framing/suggest`
- `DELETE /api/chat-framing/suggest`
- `OPTIONS /api/chat-framing/suggest`
- `GET /api/export/conversations`
- `POST /api/integrations/storage/file`
- `GET /api/integrations/storage/files{*?}`
- `DELETE /api/mcp-keys/{*}`
- `PUT /api/mcp-keys/{*}`
- `POST /api/models/autotune`
- `POST /api/models/delete`
- `POST /api/models/load`
- `POST /api/models/unload`
- `GET /api/models/machines`
- `HEAD /api/models/machines`
- `POST /api/models/machines`
- `PUT /api/models/machines`
- `PATCH /api/models/machines`
- `DELETE /api/models/machines`
- `OPTIONS /api/models/machines`
- `POST /api/models/prune-missing`
- `GET /api/models/reranker`
- `PUT /api/models/reranker`
- `DELETE /api/models/reranker`
- `GET /api/projects/{projectId}/browser/{task}`
- `GET /api/projects/{projectId}/code/{task}`
- `GET /api/projects/{projectId}/research/{job}`
- `HEAD /api/projects/{projectId}/research/{job}`
- `POST /api/projects/{projectId}/research/{job}`
- `PUT /api/projects/{projectId}/research/{job}`
- `PATCH /api/projects/{projectId}/research/{job}`
- `DELETE /api/projects/{projectId}/research/{job}`
- `OPTIONS /api/projects/{projectId}/research/{job}`
- `GET /api/projects/{projectId}/research/{job}/cancel`
- `HEAD /api/projects/{projectId}/research/{job}/cancel`
- `POST /api/projects/{projectId}/research/{job}/cancel`
- `PUT /api/projects/{projectId}/research/{job}/cancel`
- `PATCH /api/projects/{projectId}/research/{job}/cancel`
- `DELETE /api/projects/{projectId}/research/{job}/cancel`
- `OPTIONS /api/projects/{projectId}/research/{job}/cancel`
- `GET /api/projects/{projectId}/research/{job}/save`
- `HEAD /api/projects/{projectId}/research/{job}/save`
- `POST /api/projects/{projectId}/research/{job}/save`
- `PUT /api/projects/{projectId}/research/{job}/save`
- `PATCH /api/projects/{projectId}/research/{job}/save`
- `DELETE /api/projects/{projectId}/research/{job}/save`
- `OPTIONS /api/projects/{projectId}/research/{job}/save`
- `GET /api/ready`
- `PUT /api/reasoning-settings`
- `GET /api/routing-default`
- `GET /api/routing-mode`
- `HEAD /api/routing-mode`
- `POST /api/routing-mode`
- `PUT /api/routing-mode`
- `PATCH /api/routing-mode`
- `DELETE /api/routing-mode`
- `OPTIONS /api/routing-mode`
- `GET /api/routing-mode/allowed`
- `HEAD /api/routing-mode/allowed`
- `POST /api/routing-mode/allowed`
- `PUT /api/routing-mode/allowed`
- `PATCH /api/routing-mode/allowed`
- `DELETE /api/routing-mode/allowed`
- `OPTIONS /api/routing-mode/allowed`
- `GET /api/sampling-settings`
- `HEAD /api/sampling-settings`
- `POST /api/sampling-settings`
- `PUT /api/sampling-settings`
- `PATCH /api/sampling-settings`
- `DELETE /api/sampling-settings`
- `OPTIONS /api/sampling-settings`
- `GET /api/stats`
- `GET /api/usage`
- `HEAD /api/usage`
- `POST /api/usage`
- `PUT /api/usage`
- `PATCH /api/usage`
- `DELETE /api/usage`
- `OPTIONS /api/usage`
- `GET /api/usage/aggregate`
- `HEAD /api/usage/aggregate`
- `POST /api/usage/aggregate`
- `PUT /api/usage/aggregate`
- `PATCH /api/usage/aggregate`
- `DELETE /api/usage/aggregate`
- `OPTIONS /api/usage/aggregate`
