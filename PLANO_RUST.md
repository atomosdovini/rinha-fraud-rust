# Plano: rinha-fraud-rust

Novo projeto Rust para bater o `atomosdovini-cpp` (1.01ms / 5995.10) e cravar **6000 pontos** (p99 ≤ 1.00ms).

**Repositório alvo**: `~/rinha-fraud-rust/`  
**Submissão**: `ghcr.io/atomosdovini/rinha-fraud-rust:latest`

---

## Hipótese

O p99 na nuvem é latência de scheduling CFS, não de busca (~10µs). A vantagem do Rust não vem de "mais rápido que C++", mas de:

1. **Sem io_uring/epoll overhead** — thread bloqueante por conexão (thread-per-connection) já provou ser o melhor modelo (1.00ms vs 1.02ms io_uring no C++).
2. **Allocator melhor**: `mimalloc` como global allocator — menos fragmentação e latência de heap.
3. **Binário menor / startup mais rápido**: menos cold-start pressure no health check.
4. **Teto diferente**: se a arquitetura for idêntica, provável empate. Mas há levers não testados no Rust.

**Meta local**: p99 ≤ 0.36ms no k6 local (mesmo limiar que o C++ atingia antes do busy-poll).

---

## Arquitetura

```
client → :9999 (lb) → UDS SCM_RIGHTS → api1 / api2
                                          │ mmap read-only
                                          ▼
                              /index/index.bin (mesmo do C++)
```

- **lb**: processo Rust mínimo, só passa fd via SCM_RIGHTS round-robin (igual ao `lb.cpp` atual).
- **api1/api2**: processo Rust, thread-per-connection, IVF mmap'd, JSON zero-alloc, resposta HTTP manual.
- **index.bin**: reusado do C++ — mesmo formato IVF1 (magic + centroids int16[14] × 65536 + offsets + labels).

**Orçamento** (igual ao C++):
- lb: 0.10 CPU / 20 MB
- api1/api2: 0.45 CPU / 165 MB cada

---

## Stack

| Componente | Escolha | Motivo |
|---|---|---|
| Runtime | `std::thread` bloqueante | Thread-per-connection provou melhor p99 na nuvem |
| Allocator | `mimalloc` | Menos latência de heap vs glibc |
| JSON | Manual zero-alloc | Sem serde no hot path (mesmo approach do jairoblatt) |
| SIMD | `std::arch` AVX2+FMA | Mesmos kernels do C++ |
| UDS | `std::os::unix::net` | Sem dependência extra |
| HTTP | Manual | Response pré-montada como `[u8; 64]` |

**Sem tokio, sem async, sem serde.**

---

## Estrutura de arquivos

```
rinha-fraud-rust/
├── Cargo.toml
├── Dockerfile
├── docker-compose.yml          ← dev local (build:)
├── src/
│   ├── main.rs                 ← server thread-per-connection + lb mode
│   ├── index.rs                ← mmap + IVF query AVX2
│   ├── tx.rs                   ← JSON zero-alloc parser → int16[14]
│   └── lb.rs                   ← load balancer SCM_RIGHTS
└── info.json
```

---

## Fases de implementação

### Fase 1 — Scaffold + index loader (1-2h)
- [ ] `cargo new rinha-fraud-rust`
- [ ] Ler `index.bin` via `mmap` (`memmap2` crate)
- [ ] Verificar magic `IVF1`, ler centroids/offsets/labels
- [ ] Teste unitário: load + print stats

### Fase 2 — JSON parser + vectorização (2-3h)
- [ ] Parser manual em `tx.rs` — sem alloc, sem serde
- [ ] Extrair os 14 features (copiar lógica do `tx.hpp`)
- [ ] Quantizar para `int16[14]` (VECTOR_SCALE = 1/32767)
- [ ] Teste: parsear body do k6, comparar vetor com C++

### Fase 3 — Busca IVF AVX2 (2-3h)
- [ ] Portar kernel de distância L2 int16 de `index.hpp` para Rust `std::arch`
- [ ] Centroid scan (65536 × 14 int16)
- [ ] Probe dos top-N clusters
- [ ] Max-heap dos 5 vizinhos → fraud_score
- [ ] Teste: mesma query, mesmo resultado que C++

### Fase 4 — Servidor HTTP (2h)
- [ ] TCP listen em `TCP_PORT` ou UDS ctrl socket (`LISTEN`)
- [ ] Thread por conexão (exatamente como `server.cpp`)
- [ ] Parse mínimo do request: método + path + Content-Length
- [ ] `GET /ready` → `HTTP/1.1 200 OK\r\n\r\n`
- [ ] `POST /fraud-score` → pipeline tx → query → resposta JSON
- [ ] Response pré-formatada com `write!` direto no buffer de saída

### Fase 5 — Load balancer (1h)
- [ ] Portar `lb.cpp` para Rust
- [ ] TCP :9999 → SCM_RIGHTS round-robin → api sockets
- [ ] Preflight: conectar às apis antes de bind (evitar health check timeout)

### Fase 6 — Dockerfile + validação local (1h)
- [ ] Multi-stage: builder (rust:1.87-slim) → runtime (debian:trixie-slim)
- [ ] Copiar `index.bin` do stage C++ ou buildar aqui
- [ ] `make docker-up` + k6 → 0 FP/FN / 0 http_errors
- [ ] Medir p99 local — alvo ≤ 0.36ms

### Fase 7 — Submissão
- [ ] `docker build --platform linux/amd64 -t ghcr.io/atomosdovini/rinha-fraud-rust:latest .`
- [ ] `docker push`
- [ ] Atualizar branch `submission-rust` com `docker-compose.yml` + `info.json`
- [ ] Abrir issue `rinha/test atomosdovini-rust`

---

## Levers de p99 para testar na nuvem (em ordem)

1. **Thread-per-connection** (baseline, já comprovado no C++)
2. **mimalloc** (reduz jitter de heap)
3. **SO_REUSEPORT sem LB** — se a Engine aceitar (risco: pode rejeitar)
4. **`TCP_NODELAY` + `TCP_QUICKACK`** (já usado no C++, manter)
5. **Prefetch de labels** no kernel AVX2 (já no C++, portar)
6. **RERUN_CAP=1** — testar no Rust onde o bug de health check não existe

---

## O que NÃO fazer (aprendizados do C++)

- ~~io_uring / epoll~~ — pior que thread bloqueante na nuvem
- ~~SCHED_FIFO~~ — não funciona sem CAP_SYS_NICE no ambiente Rinha
- ~~cpuset / sched_setaffinity~~ — piora p99 (1.00→1.43ms)
- ~~busy-poll~~ — queima cota CFS sem ganho na nuvem
- ~~MAP_POPULATE / prefault~~ — atrasa cold-start → health check timeout

---

## Critério de subida para nuvem

Antes de gastar uma prévia:
1. `k6 run test/test.js` com 0 FP / 0 FN / 0 http_errors
2. p99 local ≤ 0.36ms (ou pelo menos não regressão severa)
3. Hipótese arquitetural clara documentada aqui
