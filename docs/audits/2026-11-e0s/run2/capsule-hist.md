# capsule histogram: run2 (all rows, scored or not)

## lme-oracle

| arm | n | median | empty | caps 0 1 2 3 4 5 6 7 8 |
|---|---|---|---|---|
| lexical | 500 | 8.0 | 0 | 0 0 2 1 4 2 2 7 482 |
| vector | 500 | 8.0 | 0 | 0 0 2 1 4 2 2 7 482 |
| fused_defaults | 500 | 6.0 | 0 | 0 104 61 42 22 18 9 6 238 |
| rerank_off | 500 | 8.0 | 0 | 0 0 2 1 4 2 2 7 482 |
| tinybert_floor_0_fill_to_cap | 500 | 8.0 | 0 | 0 0 2 1 4 2 2 7 482 |
| tinybert_floor_0.1 | 500 | 4.0 | 0 | 0 109 61 55 28 19 14 6 208 |
| tinybert_floor_0.2 | 500 | 5.0 | 0 | 0 109 67 45 21 21 9 7 221 |
| semantic_floor_0 | 500 | 6.0 | 0 | 0 104 62 41 22 18 9 6 238 |
| rrf_k60 | 500 | 8.0 | 0 | 0 122 56 24 14 11 2 1 270 |

## locomo-conv26

| arm | n | median | empty | caps 0 1 2 3 4 5 6 7 8 |
|---|---|---|---|---|
| lexical | 199 | 8 | 0 | 0 0 0 0 0 0 0 0 199 |
| vector | 199 | 8 | 0 | 0 0 0 0 0 0 0 0 199 |
| fused_defaults | 199 | 2 | 0 | 0 95 32 12 7 1 2 0 50 |
| rerank_off | 199 | 8 | 0 | 0 0 0 0 0 0 0 0 199 |
| tinybert_floor_0_fill_to_cap | 199 | 8 | 0 | 0 0 0 0 0 0 0 0 199 |
| tinybert_floor_0.1 | 199 | 2 | 0 | 0 93 38 13 8 2 2 0 43 |
| tinybert_floor_0.2 | 199 | 2 | 0 | 0 95 37 12 7 1 2 0 45 |
| semantic_floor_0 | 199 | 2 | 0 | 0 94 32 13 7 1 2 0 50 |
| rrf_k60 | 199 | 2 | 0 | 0 72 42 16 12 2 2 4 49 |
