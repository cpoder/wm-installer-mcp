# Plan d'action — wm-installer-mcp

Document vivant. Issu de l'[audit du 18 septembre 2026](audit-2026-09-18.md).
Deux sessions travaillent en parallèle sur ce dépôt : `cb`
(`wm-installer-mcp-cb`) et `b6` (`wm-installer-mcp-b6`). L'attribution ci-dessous
est une proposition ; le propriétaire du dépôt tranche.

Conventions : priorité P0 (sécurité et intégrité, avant toute nouvelle
utilisation), P1 (le flux réel de bout en bout), P2 (parité avec l'installer
officiel), P3 (plus tard). Taille S (heures), M (une journée), L (plusieurs).
Chaque item a un critère de fin vérifiable, pas seulement un test unitaire :
« vérifié » veut dire exécuté contre une installation réelle ou un artefact
IBM réel, résultat noté ici.

## Décisions prises

- **Command Central est hors périmètre** (déprécié en 12.1, décision
  utilisateur). Platform Manager reste concerné comme runtime à profil p2.
- **`installer_fetch` est abandonné** : le `.bin` vient de Passport Advantage
  ou Fix Central (IBMid), pas du download centre. `installer_check` refuse de
  démarrer avec un client trop vieux, et c'est tout ce qu'on peut faire.
- **Le modèle de confiance du chemin natif est le sha256 du catalogue
  authentifié**, pas la signature JAR. Il se documente comme tel. C'est ce qui
  fait que le natif fonctionne depuis l'expiration du certificat IBM du
  28 août 2026, et ce n'est pas un argument pour prétendre à l'équivalence.
- **Migration entre releases et configuration du runtime** (packages à
  désactiver, démarrage, arrêt) sont des candidats à un troisième serveur, pas
  des évolutions de celui-ci. Exception : le câblage JDBC de Trading Networks,
  qui est un panel de l'installer (lot 1).

## Règles de travail

- `cb` termine et commite son lot en cours (install incrémental, `config` /
  `registry` / `secrets`, `manage.rs`, `installer_check`, signatures) dès que
  `cargo test --workspace` passe ; `b6` travaille sur une branche dans un
  worktree séparé et se rebase dessus. Personne ne modifie les fichiers de
  l'autre sans le dire ; les nouveaux outils vont dans de nouveaux modules,
  l'enregistrement dans `tools.rs` est une ligne à fusionner.
- `wm12` n'est modifié que par des outils en dry run ou en lecture ;
  les vérifications qui écrivent se font sur une installation jetable
  (`native_install` dans le scratchpad, quatre minutes) ou sur une copie.
- Tout outil qui écrit a un `apply` et un dry run qui liste ses réglages et
  leur provenance. Tout outil qui lance un processus produit a un timeout et,
  au-delà de quelques secondes, devient un job.
- Un mot de passe ne va ni dans un log, ni dans `structuredContent`, ni dans
  un wrapper ; quand l'outil produit l'impose sur sa ligne de commande, le dry
  run le dit.
- Chaque item se termine par ses tests, la mise à jour des descriptions
  d'outils et des docs concernés, et une ligne « vérifié » ici.

## Lot 0 — sécurité et intégrité (P0)

| # | Item | Qui | Taille | Fichiers | Critère de fin | État |
|---|---|---|---|---|---|---|
| 0.1 | `fix_apply` : indexer les bundles livrés par (nom, version) et ne remplacer une ligne de `bundles.info` que si la version livrée est strictement supérieure ; refuser une version inférieure à celle du dépôt p2 sauf `force` ; avertir pour **tout** profil que `profiles_of` va toucher ; gérer le séparateur `\|` de `osgiShutdown` ; lire `Fix-Version` et le rapporter | b6 | M | `fix.rs`, `wm-sum-mcp/native.rs` | tests sur archive forgée avec un bundle à deux versions ; dry run sur wm12 avec `TPS.SharedBundles` 0001 puis 0003 : 0001 refusé, 0003 sans ligne dupliquée, avertissement sur MWS_default | **fait** (branche `worktree-lot-b6`, commit `585b198`). Vérifié 2026-09-18 sur wm12 en dry run : Fix 1 refusé « already at 12.1.0.0001-0731 », Fix 3 = 48 remplacements (16 par profil) au lieu de 224, 0 sur les 17 noms à double version, avertissement MWS_default en cours d'exécution ; 11 tests sur archives forgées |
| 0.2 | `image_build` / `install_run` : corriger la description (« pass them in `env` ») ; refuser dans `env` une variable dont le nom ressemble à un secret (`KEY`, `PASS`, `PWD`, `SECRET`) et renvoyer vers `passthrough_env` / le coffre ; ne jamais écrire une valeur de secret dans `run.sh` | b6 (repris de cb le 18/09) | S | `tools.rs`, `runner.rs` | test : un `env` contenant `WM_EMPOWER_KEY` est refusé ; `run.sh` d'un job ne contient aucune valeur de secret | **fait** (b6) : `environment()` refuse dans `env` un nom contenant KEY, PASS, PWD, SECRET, TOKEN ou CREDENTIAL et renvoie vers `passthrough_env` / `credential_set` ; descriptions de `image_build` et `install_run` corrigées |
| 0.3 | Update Manager hérite de l'environnement et le déverse dans `UpdateManager/logs/debug/*.log` : dans le wrapper, exécuter le programme via `env -u <VAR>` pour chaque variable de secret référencée en argument, après expansion par le shell ; même chose pour tout job piloté | b6 (repris de cb le 18/09) | S | `runner.rs` `spawn` | test : le processus enfant ne voit pas `WM_EMPOWER_KEY` dans son environnement alors que `-empowerPass` reçoit la valeur ; README « Credentials » corrigé | **fait** (b6) : `Environment::scrub` ; le wrapper fait `set -- prog args`, `unset VAR`, puis `"$@"`, et `run` / `run_console` retirent la variable du processus ; `fix_run` scrub `WM_EMPOWER_KEY`, `fixes_installed via_sum` scrub la clé et le compte ; deux tests (job détaché et exécution synchrone) ; README « Credentials » complété. Reste visible : l'argument dans la liste des processus pendant le run |
| 0.4 | `--watch` boucle sur un job natif échoué : finaliser `progress.json` (`finish(false, …)`) sur le chemin d'erreur de `run_install_job`, et faire sortir `watch()` aussi sur `exit_code` présent | b6 (repris de cb le 18/09) | S | `native.rs`, `main.rs`, `progress.rs` | test : un job dont le spec est invalide fait sortir `--watch` avec le message d'erreur | à faire |
| 0.5 | Spec de job `install-spec-<pid>.json` : un nom unique par appel (job id ou horodatage + compteur), supprimé par le job après lecture | b6 (repris de cb le 18/09) | S | `native.rs` | test : deux appels rapprochés produisent deux specs distincts | à faire |
| 0.6 | Dire ce que les outils produit font des mots de passe : dry run d'`instance_create` et de `database_configure` mentionnant « visible dans `ps` pendant l'exécution » et, pour l'instance avec base, « écrit dans `config/jdbc/properties/db.properties` » ; vérifier si `--configFile` de `dbConfigurator` peut porter le mot de passe | b6 | S | `instance.rs`, `database.rs`, `native.rs` | dry runs mis à jour ; réponse notée ici sur `--configFile` | à faire |

## Lot 1 — le flux réel de bout en bout (P1)

| # | Item | Qui | Taille | Fichiers | Critère de fin | État |
|---|---|---|---|---|---|---|
| 1.1 | Fixes installés lus nativement : parser la dernière génération de `install/fix/profile/org.eclipse.equinox.p2.engine/profileRegistry/self.profile/<ts>.profile.gz` (unités `wMFix.*`, version, `installTimestamp`) ; `fixes_installed` sans Update Manager ; `installedFixes` réels dans l'inventaire envoyé à IBM ; `fix_apply` refuse un fix déjà posé à version égale ou supérieure sauf `force` | b6 | M | `fixes.rs`, `inventory.rs`, `wm-sum-mcp/native.rs`, `wm-sum-mcp/tools.rs` | vérifié sur wm12 : 44 unités lues, mêmes noms que « View installed fixes » ; `fixes_available` ne repropose plus un fix posé | **fait** (module `fixregistry.rs`, commit sur `worktree-lot-b6`). Vérifié 2026-09-18 sur wm12 : 44 unités lues (39 fixes, 5 EM fixes), horodatage pour chacune, `installedFixes` = 44 dans la requête IBM ; `fix_apply` refuse un fix que le registre liste à version égale ou supérieure et avertit d'un `Require-Fix` absent. **Reste** : confirmer avec des identifiants IBM que `fixes_available` ne repropose plus les 44 (aucun identifiant dans la session b6) |
| 1.2 | `fixes_plan` : lire `Require-Fix`, `Install-After`, `Install-Before`, `Install-With`, `Require-SUM-Build` des manifests et `requireFix` de `content.xml` ; ordre topologique, cycles détectés, prérequis manquants nommés (installés ou sélectionnés) ; `fix_apply` accepte une liste et s'arrête au premier échec en disant ce qui est posé | b6 | M | `fix.rs`, `fixes.rs`, `wm-sum-mcp/native.rs` | vérifié sur `.work/fixes` : ordre SUMApi → TPS.SharedBundles ≥ 0002 → CCShared → SPM, et refus de SPM seul ; `CCShared` signale `OSGI.Platform ≥ 0003` absent de wm12 | à faire |
| 1.3 | Sauvegarde et désinstallation d'un fix natif : zipper les fichiers remplacés et supprimés dans `install/fix/backup/<id>_<ver>/install.phaseN_k/{replaced,deleted}_files_backup`, écrire `uninstall_instructions.txt` au format SUM (`extract(source:…)`, `delete(file:…)`, `extractRepo(source:…,repo:…)`) ; `extractRepo` reproduit fidèlement (sauvegarde puis purge du dépôt p2 avant réextraction) ; `fix_uninstall` natif qui rejoue ces instructions ; transaction : un échec en phase N restaure les phases précédentes | b6 | L | `fix.rs`, `wm-sum-mcp/native.rs` | vérifié sur install jetable : apply puis uninstall de `wMFix.SPM` rend un arbre identique (diff) ; `uninstall_instructions.txt` lisible par SUM (test : `Uninstall fixes` via `fix_run` sur ce backup) | à faire |
| 1.4 | Enregistrement pour Update Manager, en deux temps. (a) `fix_image_build` : fabriquer une image SUM (`content.jar`, `artifacts.jar`, `binary/`, `readme/`) depuis des fixes téléchargés, pour que `Install fixes from image` enregistre officiellement. (b) expérimental, derrière `register: true` : écrire une nouvelle génération de `self.profile` (unité intégrale du `content.xml`, `installTimestamp`, `lastUpdatedTime`, verrou p2), l'artefact dans `install/fix/profile/…/cache/binary` + `artifacts.xml`, le readme dans `install/fix/readme`, la ligne d'`audit.log` | b6 | L | `fixes.rs`, `fix.rs`, nouveau `sumreg.rs` | (a) vérifié : SUM installe depuis l'image et « View installed » liste le fix ; (b) vérifié : après écriture, `fixes_installed` (SUM) liste le fix et un `fix_run` ultérieur ne le réapplique pas, sans « Profile self is not current » | à faire |
| 1.5 | Instances IS : `instance_update` (`-Dpackage.list`, `-Ddb.*`), `instance_delete`, `instance_packages_remove` (`deletePackages`), et `instance_sync` qui, après une install native, liste les packages présents dans `IntegrationServer/packages` et absents d'une instance, avec dry run ; validation du nom sur le chemin Ant (pas de `..`, pas de séparateur) ; refus si l'instance tourne | cb | M | `instance.rs`, nouveau module d'outils | vérifié : le cas Deployer du 18 septembre rejoué sur install jetable (`instance_sync` propose `WmDeployer`, `apply` le copie, IS le charge) | **fait par cb** : `instance::ant::update`, `packages_not_in_instance`, outil `instance_update` (branche `feature/config-registry-credentials`) ; `instance_delete`, `deletePackages` et la validation de nom restent à faire (b6) |
| 1.6 | Licence : poser `instances/<nom>/config/licenseKey.xml` comme `LicenseWriter.copyLicenseFile` ; corriger la description de `license_file` (validé puis ignoré par `is_instance.xml` 12.1) ; le dry run dit où le fichier ira | b6 | S | `instance.rs`, `native.rs` | vérifié : IS démarre avec la licence posée, `wm.server:ping` en 200 | à faire |
| 1.7 | Câblage JDBC de Trading Networks (équivalent de `TNServerConfigPanel` + `TNServerInstallPanel`) : `jdbc_pool_create` (`config/jdbc/pool/<alias>.xml`), `jdbc_alias_set` (`config/jdbc/function/<TN\|ISCoreAudit\|…>.xml`, `cache=true` pour TN par défaut), copie de `config/Caching/webMethods-IS-TN.xml` depuis `packages/WmTN/config` ; formats relevés sur wm12 ; refus si l'instance tourne ; mot de passe chiffré comme IS le fait ou laissé à saisir | b6 | M | nouveau `jdbc.rs`, module d'outils | vérifié sur install jetable : TN démarre sans `DatastoreException` avec un pool PostgreSQL créé par l'outil | à faire |
| 1.8 | `sum_install` : télécharger le bootstrapper depuis `sum-repository-service/bootstrappers/<plateforme>/latest/<repo>` avec le bearer, l'exécuter (`sum-setup.bin -j <jvm> -d <dir> --accept-license`, proxy) en job, enregistrer `sum_home` dans le registre d'installations | b6 | M | `sdc.rs`, nouveau `sumsetup.rs`, `wm-sum-mcp` | vérifié : un SUM installé par l'outil exécute `View installed fixes` sur wm12 | à faire |
| 1.9 | Pilotage d'Update Manager : tuer le **groupe** de processus au timeout de `run_console`, drainer la sortie après la fin ; `SumWorksOnThisDir.lock` dans `stale_locks`, test de vivacité avant suppression ; `selectedFixes` vide = erreur (« Empty selectedFixes ») : validation, description, signature `diagnose_log` ; `fix_script_generate` en batch ; `backupDeletePeriod` / `periodType`, `installSP` / `spKey` / `diagnoserKey` exposés ; `fixes_installed` en job plutôt que bloquant 600 s | b6 | M | `runner.rs` (`run_console` seulement), `sum.rs`, `wm-sum-mcp/tools.rs`, `diag.rs` (une signature) | tests ; vérifié : un timeout ne laisse ni JVM ni verrou | à faire |
| 1.10 | Jobs : pid enregistré, détection de job mort, `job_list`, `job_cancel` (groupe de processus), nettoyage des specs et des vieux jobs, écriture atomique de `progress.json`, `wait` du fils ; `-scriptErrorInteract no` sur tout job de l'installer officiel et `extra_args` sur `install_run` / `image_build` | b6 (repris de cb le 18/09) | M | `runner.rs`, `native.rs`, `tools.rs`, `main.rs` | tests ; vérifié : un job tué apparaît « failed », pas « running » | à faire |
| 1.11 | Install native lisible par l'outillage IBM : `.prop` avec `installDate`, `jars`, `panels`, composants ; `.contents` avec modes ; `install/etc/installconfig.prop` ; `install/history/history.txt` et `audit.log` ; `~/.sagconf.xml` au format `SAGEnv` ; `bin/uninstall` depuis `install/etc/uninstall.template` ; README : dire ce qui manque encore | cb | L | `install.rs`, `native.rs` | vérifié : `-queryInstall` de l'installer officiel liste l'install native ; SUM « View installed fixes » s'exécute dessus | partiel : `install_verify` de cb lit les `.contents` et classe les absences (deux formats de manifeste coexistent sur wm12 : celui de l'installeur, en-tête `timestamp=` et mode octal par ligne, 163 fichiers ; le nôtre, 124). Reste : écrire `.prop` complets et `.contents` au format du vendeur, `installconfig.prop`, `history`, `~/.sagconf.xml`, `bin/uninstall` |
| 1.12 | Cache et espace : product tree validé (`product_count() > 0`) avant écriture en cache, âge exposé par `sdc_catalog`, avertissement au-delà de 7 jours ; contrôle d'espace disque avant `native_install` (`expanded_bytes` contre `install_dir`, cache et `TMPDIR`) ; verrou par home (`install/.wm-mcp.lock`) ; écriture atomique dans le cache d'artefacts | b6 (repris de cb le 18/09) | M | `native.rs`, `install.rs`, `sdc.rs` | tests ; vérifié : une page HTML en 200 n'est pas mise en cache | à faire |
| 1.13 | `plan_resolve` avec repli sur le catalogue de release ; queue de log dans le texte de `job_status` | cb | S | `tools.rs` | (déjà en cours) | en cours |
| 1.14 | Profils : dériver les roots SPM des `.prop` (`osgiProfileNames`, `osgiBundlesRepos`) ; écrire `install/profiles/<p>.data` après `profile_provision` ; canonicaliser `wm_home` partout ; `profile_capture` avec dry run ; refus d'un nom de profil contenant un séparateur | b6 | M | `profile.rs`, `native.rs` | vérifié : `profile_provision` sans `roots` provisionne SPM sur install jetable ; `<p>.data` identique à celui de l'installer | à faire |

## Lot 2 — parité avec l'installer officiel (P2)

| # | Item | Qui | Taille | Fichiers | Critère de fin | État |
|---|---|---|---|---|---|---|
| 2.1 | Désinstallation : `product_uninstall` natif depuis `install/bms/*.contents` (avec la propriété `installDir`), suppression des `.prop` et des packages d'instance, refus si instance ou profil en cours ; `uninstall_run` officiel (`bin/uninstall -readUninstallScript -console -scriptErrorInteract no`, script avec `InstallLocProducts`) | b6 | L | nouveau `uninstall.rs`, `script.rs` | vérifié : install jetable, ajout puis retrait de Deployer, arbre identique | à faire |
| 2.2 | Base de données : mode `--product` (`-pr`), actions `drop`, `recreate`, `migrate` (`--fromVersion`), `rollback`, `catalog` derrière `confirm` explicite ; `printComponents` lu ; rapport de ce que dcc a réellement fait ; résultats partiels conservés en cas d'échec ; exécution en job avec timeout ; vocabulaire `--dbms` vérifié par moteur | b6 | M | `database.rs`, `native.rs` | vérifié : `create -pr TN` sur PostgreSQL jetable équivaut au runbook | à faire |
| 2.3 | Instance MWS (`mws.sh new` puis `init`), realm Universal Messaging (`ninstancemanager.sh`), `instance_registry.xml` et `persist.prop` mis à jour comme les panels le font | b6 | L | `instance.rs` ou nouveaux modules | vérifié : MWS répond sur 8585 après `mws_instance_create` + `profile_provision` | à faire |
| 2.4 | Switches de l'installer : `image_validate` (`-validateImage`), `image_contents`, `-acceptInnovationRelease` + signature « Innovation release is not accepted », `-masterPassword` et `@secure@` (lecture et écriture), `InstallLocProducts` préservé, échappement `.properties`, contrôle `LicenseAgree`, avertissement sur un placeholder contenant un chiffre (bug `[A-Z0-0a-z_]` de l'installer) | cb | M | `tools.rs`, `script.rs`, `diag.rs` | tests ; vérifié : une image construite est validée par l'installer | à faire (le contrôle de version du client est fait par cb : `wm_core::client`, refus dans `install_run` / `image_build`) |
| 2.5 | Réseau d'entreprise : proxy explicite (`WM_PROXY` / `HTTPS_PROXY`, SOCKS, auth) et CA d'entreprise (`WM_CA_BUNDLE`) dans `sdc.rs` ; `-proxy*` / `-SSLcacert` relayés à l'installer, `-proxy*` à Update Manager ; timeout par octet plutôt que global, retry sur erreur transitoire, re-handshake sur 401 | b6 | M | `sdc.rs`, `tools.rs`, `sum.rs` | tests avec un proxy local ; vérifié : téléchargement d'un artefact via proxy | à faire |
| 2.6 | Recettes de fix : parser tous les arguments (`exclude`, `targetProduct`, `targetPlatform`, `deferred`, tokens `${install.dir}`…), phases `uninstall.*`, `extractNoBackup`, `replace`, `setProperty` / `deleteProperty`, `createSymLink`, `startScript` derrière `confirm`, `installISPackage` ; reprovisionner par le director les profils dont `<p>.data` référence un dépôt touché (équivalent de `platform_recreator_phase`) | b6 | L | `fix.rs`, `profile.rs` | vérifié : un fix IS et un fix MWS de wm12 s'appliquent sans action « not performed » | à faire |
| 2.7 | Windows : soit un runner de jobs Windows (`cmd` / PowerShell, pas de `setsid`), `USERPROFILE`, `.bat` des outils produit ; soit retirer les cibles Windows de `release.yml` et le dire dans le README. Recommandation : retirer jusqu'à ce que quelqu'un en ait besoin | b6 (repris de cb le 18/09) | S ou L | `release.yml`, README, `runner.rs` | décision notée ici | à faire |
| 2.9 | Vérification d'après-coup (`install_verify`), IS démarré : `wm.server:ping`, packages en erreur de chargement, alias de serveur distant testés un par un (l'alias `local` livré par défaut échoue en `Invalid credentials` et bloque Deployer, dont le message parle d'autorisation dans sa console ; retour de la session `wm12-as2-demo-be` du 18 septembre), profils p2 qui répondent ; rapport, aucune modification | b6 | M | nouveau `verify.rs` | vérifié sur wm12 : l'alias `local` est signalé | à faire |
| 2.8 | Cohérence des arguments : un seul résolveur d'installation (`install` / `install_dir` / `wm_home`, registre, `$WM_HOME`, défaut configuré) partagé par les deux serveurs ; `apply` sur tout outil qui écrit ; `Defaults` honorés par `wm-sum-mcp` ; descriptions relues ; les helpers dupliqués (`tail`, `install_dir`, filtre de recherche, login → sandbox) réunis dans `wm-core` | b6 après le commit de cb | M | les deux `tools.rs`, `native.rs` | tests de la couche MCP ; grep : plus de définition dupliquée | à faire |

## Lot 3 — plus tard (P3)

| # | Item | Remarque |
|---|---|---|
| 3.1 | Image de conteneur (équivalent de `create container-image`) | génération d'un Dockerfile depuis une install native ; utile pour MSR |
| 3.2 | Détection des exécutables par magic number au déballage | l'installer force 0755 sur un binaire sans mode déclaré |
| 3.3 | Migration entre releases, configuration du runtime | troisième serveur |
| 3.4 | Language packs, services Windows, menu Démarrer | jamais demandé |

## Socle transverse (P1)

| # | Item | Qui | Critère de fin |
|---|---|---|---|
| T.1 | Faux download centre pour les tests (petit serveur HTTP dans `tests/`, arbre produit et artefacts forgés) ; `unpack` et `fetch` testés de bout en bout ; archives de fix forgées pour `fix::apply` ; profil de test avec un bundle à deux versions | b6 | `cargo test` couvre téléchargement, digest divergent, déballage, application et désinstallation d'un fix |
| T.2 | Tests de la couche MCP : dispatch de `mcp-rt`, `isError`, lecture des arguments, un test par outil sur son dry run | b6 (repris de cb le 18/09) | chaque outil a au moins un test de dry run |
| T.3 | Docs : corriger `p2-profiles.md` (verbes inexistants), `sum-protocol.md` (`openpty`), `install-panels.md` (états historiques marqués, « now native » retiré), `database.rs` et `instance.rs` (docs de module), README (`PortalStartConfiguratorSerenity`, ce qu'une install native ne produit pas, Windows), `main.rs` (toutes les variables d'environnement) | b6 (sum, provisioning) / cb (README, installer) | relecture croisée |

## Ordre de démarrage proposé

`b6` : 0.1 → 1.1 → 0.3 → 0.2 → 0.4 → 0.5 → 1.9 → 1.6 → 1.7 → 1.8 → 1.2 → 1.3 → 1.10 → 1.12 → T.1 → 1.14 → 2.2 → 2.1 → 2.5 → 2.6 → 1.4 → 2.3 → 2.8 → 2.7 → T.2 → T.3.

`cb` : terminer le lot en cours → 0.2 → 0.3 → 0.4 → 0.5 → 1.10 → 1.12 → 1.11 → 2.4 → 2.7 → T.2 → T.3.

## Journal

- 2026-09-18 : audit livré, plan écrit, découpage proposé à `cb`.
- 2026-09-18 : 0.1 et 1.1 faits par `b6` sur la branche `worktree-lot-b6` (worktree `.claude/worktrees/lot-b6`). Les deux documents `docs/audit-2026-09-18.md` et `docs/plan.md` sont versionnés sur cette branche ; les copies non suivies laissées dans le checkout principal sont à supprimer avant la fusion.
- 2026-09-18 : `cb` a commité son lot sur `feature/config-registry-credentials` (7 commits, tête `5fe1b96`, 165 tests, `cargo audit` propre après rustls 0.23.45) et rend les items 0.2, 0.3, 0.4, 0.5, 1.10, 1.12, 2.7 et T.2, repris par `b6`. `b6` se rebase sur cette branche.
- 2026-09-18 : 0.2 et 0.3 faits par `b6` ; les chemins de cette machine ont été retirés des deux documents (dépôt public, convention `/opt/webmethods` / `wm12`).
