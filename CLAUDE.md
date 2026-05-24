# rinha-fraud-rust

Serviço de detecção de fraude em **Rust** para a Rinha de Backend 2026. Port do `atomosdovini-cpp` (C++20, p99 ~1.00ms / 5999 pts) com a hipótese de que o Rust oferece levers não explorados no C++ para cravar **6000 pontos** (p99 ≤ 1.00ms).

Mesmo problema, mesma engine de índice (IVF `index.bin` reutilizado do C++), mesmo modelo arquitetural thread-per-connection.

---

## Objetivo

Cravar 6000 pontos na nuvem (Mac Mini Late 2014, 2 núcleos, amd64). Fórmula: `1000·log₁₀(1000/max(p99_ms,1.0))` — satura em 3000 quando p99 ≤ 1ms. Score total = detecção (3000/3000) + p99_score. Empate em 1º = 6000.

**O resultado que conta é só o da nuvem.** Medição local não prevê p99 da nuvem — usar k6 local apenas para confirmar 0 FP/FN/erros antes de gastar uma prévia.

---

## Por que Rust (hipótese)

O p99 ~1ms na nuvem é latência de scheduling CFS, não de busca (~10µs). O C++ já resolve o pior ofensor (thread-per-connection > io_uring). Levers adicionais no Rust:

1. **`mimalloc`** como global allocator — menos jitter de heap vs glibc
2. **Binário menor / startup mais rápido** — menos pressão no health check
3. **`RERUN_CAP=1`** — o cap de re-run do IVF não foi testado na nuvem no C++ (risco de health check timeout lá); no Rust o cold-start está resolvido
4. **SO_REUSEPORT sem LB** — testável (o C++ submission voltou a ter LB por compliance; aqui podemos arriscar novamente)

---

## Aprendizados do C++ (não repetir)

| Experimento | Resultado nuvem | Conclusão |
|---|---|---|
| io_uring vs thread-per-connection | 1.02 → 1.00ms | TPC é melhor; reactor único tem HoL blocking |
| busy-poll | sem ganho | queima cota CFS sem benefício |
| PM QoS | sem ganho na nuvem | só ajudava local (C-states) |
| SCHED_FIFO | 1.02ms (pior) | exige CAP_SYS_NICE — proibido na Rinha |
| cpuset / sched_setaffinity | 1.00 → 1.43ms | pin piora: worker fica esperando core ocupado pelo k6 |
| BVH exata | 1.02ms (pior que IVF) | o re-run IVF não é o gargalo dominante |
| MAP_POPULATE / prefault | health check timeout | atrasa cold-start além do timeout de 5s |

**Regra de ouro da nuvem**: o p99 é jitter de CFS com 3 processos CPU-bound em 2 cores (k6 + api1 + api2, cada container com cota 0.5 CPU). Qualquer técnica que consuma cota extra em spin/wait piora o p99.

---

## Arquitetura

```
client → :9999 (lb) → UDS SCM_RIGHTS → api1 / api2
                                          │ mmap read-only
                                          ▼
                              /index/index.bin (IVF, mesmo do C++)
```

**Dois modos de operação** (controlados por env var, igual ao C++):
- `TCP_PORT=9999` — bind TCP direto, sem LB (dev local / SO_REUSEPORT)
- `LISTEN=/sockets/api*.sock` — UDS ctrl socket, recebe fds via SCM_RIGHTS do lb (submission)

**Orçamento** (submission):
- lb: 0.10 CPU / 20 MB
- api1/api2: 0.45 CPU / 165 MB cada

---

## Stack

| Componente | Escolha |
|---|---|
| Runtime | `std::thread` bloqueante (thread-per-connection) |
| Allocator | `mimalloc` (`mimalloc` crate, global allocator) |
| JSON | Parser manual zero-alloc (sem serde no hot path) |
| SIMD | `std::arch` AVX2+FMA (kernels portados do C++) |
| UDS | `std::os::unix::net` |
| HTTP | Manual — response pré-formatada em buffer `[u8; 128]` |

**Sem tokio, sem async, sem serde.**

---

## Estrutura de arquivos

```
src/
├── main.rs      — entry point, thread-per-connection, routing
├── index.rs     — mmap IVF, query AVX2, fraud_score
├── tx.rs        — JSON zero-alloc parser → int16[14]
└── lb.rs        — load balancer TCP→UDS SCM_RIGHTS
```

---

## Hot path (POST /fraud-score)

1. `tx.rs::extract()` — parse JSON → `[i16; 14]`, zero-alloc
2. `index.rs::query()` — fase 1: scan 65536 centroids SIMD → top-nprobe shortlist; fase 2 (re-run): probe clusters restantes se resultado for "extremo"
3. Resposta HTTP manual com `fraud_score` formatado no buffer

---

## Tuning (env vars em docker-compose.yml)

Mesmas que o C++:

- `NPROBE` / `FAST_NPROBE` — clusters IVF varridos
- `ADAPTIVE_MIN` / `ADAPTIVE_MAX` — faixa que não dispara re-run
- `RERUN_CAP` — cap de clusters no re-run (1 = desabilita re-run efetivamente)
- `INDEX_MMAP=1` — mapeia índice compartilhado entre réplicas
- `MALLOC_ARENA_MAX=1` — arenas glibc enxutas (fallback se mimalloc não for ativado)

---

## Build & teste

```bash
make docker-up      # build + sobe lb, api1, api2 (porta 9999)
make docker-down

# Benchmark oficial:
cd ~/rinha-de-backend-2026 && k6 run test/test.js
jq '{p99, final:.scoring.final_score, bd:.scoring.breakdown}' test/results.json
```

---

## Submissão (requisitos obrigatórios)

Idênticos ao C++ — verificados na prática:

- **Sem `build:`** no docker-compose.yml da branch `submission-rust` — usar `image: ghcr.io/atomosdovini/rinha-fraud-rust:latest`
- **Imagem pública** no GHCR (package marcado como público)
- **`--platform linux/amd64`** no build (host da Rinha é amd64)
- **Branch `submission-rust`** com apenas `docker-compose.yml` + `info.json`
- **Prévia**: abrir issue em `zanfranceschi/rinha-de-backend-2026` com `rinha/test atomosdovini-rust`

⚠️ A Engine roda a imagem do GHCR — push obrigatório antes de abrir issue:

```bash
docker build --platform linux/amd64 --no-cache \
  -t ghcr.io/atomosdovini/rinha-fraud-rust:latest .
docker push ghcr.io/atomosdovini/rinha-fraud-rust:latest

# Verificar que a imagem nova está no GHCR:
docker rmi ghcr.io/atomosdovini/rinha-fraud-rust:latest
docker pull ghcr.io/atomosdovini/rinha-fraud-rust:latest
docker run --rm --entrypoint ls ghcr.io/atomosdovini/rinha-fraud-rust:latest \
  -la /server /index/index.bin
```

⚠️ **Limite: 10 prévias por dia.** Antes de gastar uma:
1. k6 local com 0 FP / 0 FN / 0 http_errors
2. Hipótese arquitetural clara documentada
3. Agrupar mudanças independentes de baixo risco numa única prévia

---

## Levers de p99 para testar (em ordem de prioridade)

1. **mimalloc** — baseline; se não mover, descarta como lever
2. **RERUN_CAP=1** na nuvem — cap de re-run; local mostrou 0 FP/FN com cap=1 no C++
3. **SO_REUSEPORT sem LB** — se a Engine aceitar (risco de compliance)
4. **`TCP_NODELAY` + `TCP_QUICKACK`** — manter do C++

---

## Referências

- Repo da Rinha: https://github.com/zanfranceschi/rinha-de-backend-2026
- Placar ao vivo: https://rinhadebackend.com.br/
- Repo C++ (referência): `~/rinha-fraud-cpp` (branch `main` + `submission`)
- Formato do índice: `~/rinha-fraud-cpp/src/index.hpp` — magic `IVF1`, centroids `int16[14]×65536`, offsets, labels
