# capsule histogram: run1 (all rows, scored or not)

## lme-oracle

| arm | n | median | empty | caps 0 1 2 3 4 5 6 7 8 |
|---|---|---|---|---|
| lexical | 500 | 8.0 | 0 | 0 0 2 1 4 2 2 7 482 |
| vector | 500 | 8.0 | 0 | 0 0 2 1 4 2 2 7 482 |
| fused_defaults | 500 | 6.0 | 0 | 0 107 58 42 24 18 9 6 236 |
| rerank_off | 500 | 8.0 | 0 | 0 0 2 1 4 2 2 7 482 |
| tinybert_floor_0_fill_to_cap | 500 | 8.0 | 0 | 0 0 2 1 4 2 2 7 482 |
| tinybert_floor_0.1 | 500 | 4.0 | 0 | 0 111 58 57 29 19 14 6 206 |
| tinybert_floor_0.2 | 500 | 5.0 | 0 | 0 108 66 47 21 21 9 7 221 |
| semantic_floor_0 | 500 | 6.0 | 0 | 0 107 59 41 24 18 9 6 236 |
| rrf_k60 | 500 | 8.0 | 0 | 0 118 53 26 13 12 2 1 275 |

## locomo-conv26

| arm | n | median | empty | caps 0 1 2 3 4 5 6 7 8 |
|---|---|---|---|---|
| lexical | 199 | 8 | 0 | 0 0 0 0 0 0 0 0 199 |
| vector | 199 | 8 | 0 | 0 0 0 0 0 0 0 0 199 |
| fused_defaults | 199 | 2 | 0 | 0 93 36 11 7 1 2 0 49 |
| rerank_off | 199 | 8 | 0 | 0 0 0 0 0 0 0 0 199 |
| tinybert_floor_0_fill_to_cap | 199 | 8 | 0 | 0 0 0 0 0 0 0 0 199 |
| tinybert_floor_0.1 | 199 | 2 | 0 | 0 95 39 13 8 2 2 0 40 |
| tinybert_floor_0.2 | 199 | 2 | 0 | 0 93 39 12 7 1 2 0 45 |
| semantic_floor_0 | 199 | 2 | 0 | 0 93 36 11 7 1 2 0 49 |
| rrf_k60 | 199 | 2 | 0 | 0 66 49 15 14 1 5 2 47 |
