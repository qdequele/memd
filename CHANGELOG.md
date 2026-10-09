# Changelog

## [0.4.1](https://github.com/qdequele/memd/compare/v0.4.0...v0.4.1) (2026-10-09)


### Bug Fixes

* **deps:** restore unicode-normalization dropped by the 0.4.0 release commit ([f352127](https://github.com/qdequele/memd/commit/f352127ec1023137ff46845fc67e7fb660ab0279))
* **deps:** restore unicode-normalization dropped by the 0.4.0 release commit ([0072c0d](https://github.com/qdequele/memd/commit/0072c0d966b7a731e7a2db8b9ea8cad18429641a))

## [0.4.0](https://github.com/qdequele/memd/compare/v0.3.0...v0.4.0) (2026-10-09)


### Features

* **agents:** ship memd skills + a self-healing SessionStart hook ([c7aefb3](https://github.com/qdequele/memd/commit/c7aefb3cdd4e9f9d30a74ec6554a450da4498640))
* **agents:** ship memd skills + a self-healing SessionStart hook ([1159449](https://github.com/qdequele/memd/commit/11594495e828381f7dcf1a39f632482cb3ea0f12))
* **cli:** entity, relate, unrelate, entity filters, project header, back-fill ([9f646f0](https://github.com/qdequele/memd/commit/9f646f07ba63c4cb8aa202ab0950fc8ef28bd532))
* **crawler:** project entities from git repositories, linked crawled files ([4e465ce](https://github.com/qdequele/memd/commit/4e465ce9997bd11a652494f6718e77a14db066a4))
* **knowledge:** explore an entity's relations, neighbours and memories ([4ffd377](https://github.com/qdequele/memd/commit/4ffd377938ad3b440241685c586c0991c168d3ca))
* **knowledge:** id-first name resolution with scoped alias tie-break ([2bfcb5b](https://github.com/qdequele/memd/commit/2bfcb5bc5cb9997bc5c465f50f53bed4eddb217d))
* **knowledge:** identity primitives for entities and relations ([f570fc0](https://github.com/qdequele/memd/commit/f570fc0c2c391242633327e34d015aafd5c04f2d))
* **knowledge:** link memories to mentioned entities and their project ([fab36dc](https://github.com/qdequele/memd/commit/fab36dc31d7896317b6c7ccdc1395ac5cce38f3f))
* **knowledge:** memory_relations index, relation store and events ([2f6e58c](https://github.com/qdequele/memd/commit/2f6e58ca02a11adfb23b87077db674a91b12ff72))
* **knowledge:** save entities with merge semantics, relate, forget relations ([24f93f0](https://github.com/qdequele/memd/commit/24f93f0f9e444e62cf9a169f24f7b112a191bec8))
* **mcp:** save_entity, explore, forget_relation; entity filters and links ([def4120](https://github.com/qdequele/memd/commit/def41209d55c31c79cc2fcef8564b7e765f4df6a))
* **memory:** knowledge fields, entity type, entity/status/kind filters ([560b149](https://github.com/qdequele/memd/commit/560b1495acaca180b2d150e633dddcc07f969471))
* typed records and relations (knowledge base, sub-project 1) ([773a498](https://github.com/qdequele/memd/commit/773a49862a0dcf0ae53437e7850a99ddc67b1573))


### Bug Fixes

* **context:** keep session injection project-specific and agent-aware ([93cd75b](https://github.com/qdequele/memd/commit/93cd75b3c4e0e5e5870420dbbf096533798eb9c0))
* **daemon:** bound the log, quiet the engine, and self-heal a wedged Meilisearch ([aefe290](https://github.com/qdequele/memd/commit/aefe290e96353ce736401220a1bc3b4bd3920654))
* **daemon:** bound the log, silence per-request engine noise, self-heal a wedged engine ([9673d8f](https://github.com/qdequele/memd/commit/9673d8fd2e11e154f4d5d6102165ce052ef6c546))
* **knowledge:** protect agent entities from the crawler; validate before writing ([f815d76](https://github.com/qdequele/memd/commit/f815d760aa138ef22398cd17199880949270403b))
* **memory:** make recall useful across agents — scopes, crawler noise, watcher storm, access bump ([de7beb1](https://github.com/qdequele/memd/commit/de7beb1fbdf452c7ffd9bd5ab4fd101662151ebf))
* **memory:** make recall useful across agents — scopes, crawler noise, watcher storm, access bump ([a55c2ac](https://github.com/qdequele/memd/commit/a55c2acbb83053f4bbefa82d7a9a6ca34a030723))

## [0.3.0](https://github.com/qdequele/memd/compare/v0.2.0...v0.3.0) (2026-06-16)


### Features

* agent auto-setup with interactive picker ([90cecff](https://github.com/qdequele/memd/commit/90cecff68ba072d52e843c31e4df2833f085ecf7))
* **agents:** registry, detection, status, and MCP merge helpers ([bf49eef](https://github.com/qdequele/memd/commit/bf49eef3e63ab1343244947192438aa8c1671b3d))
* **history:** audit timeline of memory changes ([722cad5](https://github.com/qdequele/memd/commit/722cad55cb28c4126e00624ea5cbd77de7f49872))
* **history:** audit timeline of memory changes ([08cadff](https://github.com/qdequele/memd/commit/08cadffe538817c9b473a4e1ff8adda582c363f3))
* **setup:** interactive agent picker with sync semantics ([cec834d](https://github.com/qdequele/memd/commit/cec834d3f0053632e34766d4392e03881da0227e))
* **status:** report per-agent memd wiring state ([19c02e4](https://github.com/qdequele/memd/commit/19c02e42f1f904247541a26000c50458185e070e))


### Bug Fixes

* **agents:** never overwrite an unparseable agent config (data-loss guard) ([5c76069](https://github.com/qdequele/memd/commit/5c76069faca4291ca97d3e935bcb8edf84041d4e))
* **setup:** drop duplicate post-setup message ([1ee4ba7](https://github.com/qdequele/memd/commit/1ee4ba72a9c5324765c1d98b3ee6d31ce4ee6dc4))

## [0.2.0](https://github.com/qdequele/memd/compare/v0.1.0...v0.2.0) (2026-06-11)


### Features

* **update:** add [update] config section with auto=true default ([fe253cd](https://github.com/qdequele/memd/commit/fe253cd22a1cf99cae672c9e82501101f44b1853))
* **update:** add update state/marker/lock/dumps path helpers ([bf35c28](https://github.com/qdequele/memd/commit/bf35c28aa2a29f3f340ad9746893f08495e655a0))
* **update:** daemon applies pending migrations and runs the daily updater ([bd594a9](https://github.com/qdequele/memd/commit/bd594a9d0bb8abdd89eda85509ec132cc24d924e))
* **update:** daily check loop applying at most one update per tick ([7fc95bb](https://github.com/qdequele/memd/commit/7fc95bbaa0f5d6a35b41f79eebf7dc6cc501f440))
* **update:** engine migration prepare/apply with rollback and backup pruning ([b0571ca](https://github.com/qdequele/memd/commit/b0571cac0522dfeec6ce525b14a65109ed707011))
* **update:** GitHub release checker with semver compare and decide() ([47658a6](https://github.com/qdequele/memd/commit/47658a6ed3fdefa03a275f0a903c9a8227d0e00e))
* **update:** memd update [--check] command and status reporting ([f534f7f](https://github.com/qdequele/memd/commit/f534f7f50f9cce18dc4e5357f421608c66a09bf2))
* **update:** self-updater with --version sanity check and atomic swap ([a688199](https://github.com/qdequele/memd/commit/a68819988dcc7ff9fd313483a6481b29188660c3))
* **update:** UpdateState persistence and exclusive UpdateLock ([9ba77a7](https://github.com/qdequele/memd/commit/9ba77a775808d1ff4effdc7bbf4abe2646a90bbb))
* **update:** version-addressed engine download, --import-dump spawn, create_dump ([072d18c](https://github.com/qdequele/memd/commit/072d18cc98851e95b75c09521ff62523eecdb991))


### Bug Fixes

* **update:** doctor-backup isolation, honest offline ticks, rollback on config-save failure ([98cd339](https://github.com/qdequele/memd/commit/98cd339d1733e947d4031bdcf60c8d35364914e5))
* **update:** flush logs before re-exec; drop unused save; document is_installed caveat ([d14b038](https://github.com/qdequele/memd/commit/d14b03887b72ae2dd1a45e482eccb66f1f72c9cc))
* **update:** honest network-failure reporting, health-verified restart, clippy clean ([dea6b13](https://github.com/qdequele/memd/commit/dea6b134f3a231baa6f55e759815c8d565702620))
* **update:** non-blocking verify with timeout, https-only download, temp cleanup ([f5118a9](https://github.com/qdequele/memd/commit/f5118a9e178799ffaa32c34c2086b880b817992e))
* **update:** retry binary verification spawn on ETXTBSY ([5a5814a](https://github.com/qdequele/memd/commit/5a5814aed9b2b6c9c98c9f2386c8c8b52d3f6984))
* **update:** self-healing rollback, stale-marker expiry, stricter verification ([cf76c5b](https://github.com/qdequele/memd/commit/cf76c5b6d84651500c89056f0553f7854cae156e))
* **update:** surface GitHub error body; cover draft-release skip ([042725b](https://github.com/qdequele/memd/commit/042725b01d32ee7f40edb5fd0f71ba320578753a))
* **update:** treat future-dated lock mtime as stale; review polish ([f41c2de](https://github.com/qdequele/memd/commit/f41c2ded7550771887f8480e46ad2928e5e3a356))
