# Machine Execution Exchange (MEE)

**하이퍼리퀴드 구조의 perp 거래소를 솔라나 생태계에. 그리고 솔라나 런치패드에서 졸업한 토큰은 자동으로 perp가 된다.**

- 매칭 엔진이 곧 BFT 체인이고, 자금은 솔라나 브릿지에 있다. 사용자는 솔라나 지갑 키로 주문에 서명한다.
- HIP-3처럼 누구나 스테이크를 걸고 자기 perp DEX를 열 수 있다.
- 그보다 한 걸음 더: **런치패드 본딩커브 → 졸업 → 잠긴 유동성 AMM → 검증인이 증명한 TWAP → 자동 perp 상장.** 토큰 제작자가 perp 수수료를 받는다.

| 구성 | 경로 | 설명 |
|---|---|---|
| CEX급 엔진 | [`crates/mee-engine`](crates/mee-engine) | 오더북, 증거금/청산/펀딩 TWAP/ADL, 여러 DEX, 머클 상태 커밋먼트. 단일 스레드 약 173만 ops/s |
| 체인 상태 머신 | [`crates/mee-chain`](crates/mee-chain) | 서명 트랜잭션, 취소 우선 블록 순서, 브릿지, 런치 상장/오라클, 스테이킹/에폭, 계정 증명. 4코어 약 12만 tx/s |
| BFT 합의 | [`crates/mee-consensus`](crates/mee-consensus) | HotStuff 2-chain (Jolteon식), 리더 평판, 커밋 증명, 장애 시뮬레이터 |
| 검증인 노드 | [`crates/mee-node`](crates/mee-node) | 합의 → 체인, 디스크 영속성/크래시 복구, 에폭 전환, 검증된 상태 동기화 |
| 네트워크 + API | [`crates/mee-net`](crates/mee-net) | 인증 TCP P2P, 가십, 동기화, HTTP/JSON + WebSocket, `mee-validator` 바이너리 |
| 솔라나 프로그램 | [`programs/mee`](programs/mee) | 브릿지(입금, 출금 승인 + 이의제기 기간 + 잠금, 검증인 교체), 런치패드(본딩커브, 졸업, TWAP AMM), 온체인 HIP-3 DEX |
| 공용/온체인 엔진 | [`crates/mee-core`](crates/mee-core) | 공용 수학, 파라미터, 고정 크기 온체인 엔진 |
| 설계 문서 | [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) | 구조, 측정값, 프로토콜 규칙, 회계 공식, 한계 |

## 실행

```bash
cargo test --workspace
cargo bench -p mee-engine   # 엔진 처리량
cargo bench -p mee-chain    # 서명 검증 포함 체인 처리량

# 로컬 4검증인 devnet (P2P 26600+i, API 8600+i)
cargo build --release -p mee-net
for i in 0 1 2 3; do ./target/release/mee-validator --index $i --data ./data & done
curl localhost:8600/status
```

툴체인은 `rust-toolchain.toml`(1.97.0)에 고정돼 있다. 아직 프로토타입이고 감사 전이다. 남은 일은 [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md#13-한계와-다음-단계)에 정리했다.
