# ADR 0001 — Contrato de montaje y red de memoria

- **Estado:** Proposed
- **Fecha:** 2026-11-05
- **Alcance:** diseño. Este documento no cambia código, MCP, el store ni el transporte.

Kimetsu guarda un brain por repo en SQLite y lo entrega al agente por MCP. Este ADR fija el contrato de montaje de esos brains, las operaciones que lo respetan, los packs firmados y un bus P2P para mover packs. El bus no clona repositorios. La ruta por defecto sigue sin LLM.

## Contexto

Hoy el cerebro ya tiene piezas que este contrato tiene que nombrar sin redefinirlas:

- **Brains.** Un SQLite por proyecto y un user-brain entre proyectos para las preferencias globales. El warm-start abre ese user-brain en solo lectura. Kimetsu Remote puede añadir un org-brain compartido dentro de un namespace; escribirlo exige token global. El enum `MemoryScope` admite `global_user`, `project`, `repo` y `run`. `session`, `org` y `skill` todavía no son niveles de ese enum: la sesión vive en el carril de resume, la org en el checkpoint remoto y el skill en las lecciones que ya se citan lo bastante como para graduarse.
- **Packs.** `kimetsu brain export` / `import` producen un sobre `kimetsu_pack` comprimido con gzip, con scrub de credenciales y PII. El merge es aditivo e idempotente. `--mode replace` hace supersede: no borra, y se puede deshacer. Un pack en cuarentena llega a la cola de revisión y no entra en retrieval.
- **Promote / decay.** Una memoria citada cuando de verdad cambia el trabajo sube (promote). El consejo viejo y el pasajero silencioso bajan por una vida media (decay). El audit de memoria de agentes ya señaló el fallo de tratar toda cita como éxito.
- **MCP y warm-start.** El borde con Claude Code, Codex, Pi, OpenClaw y Cursor es MCP. El warm-start arma un digest acotado, sin modelo y sin red, y separa el digest de repo del carril de resume de la tarea. Una caché de texto no sustituye el resultado recién armado. `context` explícito por MCP conserva el reporte de evidencia parcial.
- **Durabilidad.** El log de eventos es la fuente y el proyector reconstruye la proyección, FTS incluido. El rebuild toma el writer lock de SQLite. El sync personal replica ese log por directorio, sin servidor, y deja fuera la telemetría y los eventos `run.*`. Kimetsu Remote publica checkpoints (SQLite con WAL, Postgres, Redis o DynamoDB) y reconstruye un SQLite desechable por request. `@kimetsu-ai/sdk` es el cliente TypeScript de esa superficie MCP.

Lo que falta es una regla única de nivel. Sin ella, un merge, un replay o un import pueden dejar un recuerdo de un brain dentro de otro. El audit de parámetros ya mostró que una consolidación por similitud cruza scopes cuando el merge no exige el mismo scope. Aquí eso tiene nombre: contaminación, y es un bug.

Compartir sigue partido en tres mecanismos que no hablan el mismo contrato: el fichero de pack, el directorio de sync y el servidor HTTP de Kimetsu Remote. Ninguno firma el alcance del pack. Ninguno es un bus de presencia. El ingest con checkout (`--repos-file`, `--checkout-dir`) copia árboles de trabajo y queda fuera de este diseño.

## Decisión

### 1. Contrato de montaje

Un montaje es `(nivel, identidad, concesión)`.

| Nivel | Identidad | Qué guarda | Qué no guarda |
| --- | --- | --- | --- |
| `run` | un run del agente | episodios y telemetría de esa ejecución | nada que se exporte en un pack |
| `session` | tarea / sesión / worktree | el carril de resume: títulos y progreso | el digest compartido del repo |
| `repo` | un repositorio | el brain de proyecto, un SQLite | preferencias globales de la persona |
| `org` | un grupo de confianza (namespace) | el org-brain compartido por los repos de ese grupo | otros namespaces |
| `user` | una persona | el user-brain, entre proyectos | el org-brain |
| `skill` | un skill graduado | las lecciones que justifican ese skill | el cuerpo `SKILL.md` de otro host, y el transcript |

`project` en `MemoryScope` es el mismo montaje que `repo`. Se acepta como alias de entrada hasta una migración explícita. Deja de ser un segundo nivel.

La concesión (`grant`) es `read` o `write`. `write` incluye `read`. Un montaje cerrado no se lee y no se escribe. La concesión la pone quien abre el brain: la CLI, la sesión MCP, el token o el roster. En Kimetsu Remote, el token de repo es `write` sobre `repo` y `read` sobre `org` cuando el org-brain compartido está activo. El token global es `write` sobre `org`. El token de repo se queda en `read` sobre `org` y no abre `user`.

Reglas de acceso:

- `context` y `search` leen solo montajes con `read`. Cada recuerdo vuelve etiquetado con su nivel.
- `record`, `cite` y `supersede` nombran un único montaje y exigen `write` en ese montaje.
- Promote y decay cambian la utilidad y conservan el nivel.
- Subir de nivel — de `session` a `repo`, de `repo` a `org` o `user`, de una lección citada a `skill` — es otro `record` o un `import_pack`, con concesión propia en el destino.
- El warm-start abre el user-brain en `read`. El digest de repo y el carril de `session` no se mezclan.

**La contaminación es un bug.** Hay contaminación cuando un recuerdo aceptado aparece en un montaje distinto del suyo sin un `record` o un `import_pack` en el destino, cuando una lectura devuelve un montaje sin `read`, o cuando una escritura toca un montaje sin `write`. Merge, replace, replay, warm-start, MCP y el bus caen bajo la misma regla. Un test que cruce niveles y pase es un test roto.