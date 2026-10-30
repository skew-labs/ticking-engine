# Machine Execution Exchange (MEE)

**하이퍼리퀴드 구조의 perp 거래소를 솔라나 생태계에.**
매칭 엔진이 곧 BFT 체인이고, 자금은 솔라나 브릿지에 있다. 사용자는 솔라나 지갑 키로 주문에 서명한다.
HIP-3처럼 누구나 스테이크를 걸고 자기 perp DEX(주식, 원자재 등)를 열 수 있다.

| 구성 | 경로 | 설명 |
|---|---|---|
| CEX급 엔진 | [`crates/mee-engine`](crates/mee-engine) | 메모리 오더북, 증거금/청산/펀딩/정산, 여러 HIP-3 DEX. 단일 스레드 약 229만 ops/s |
| 체인 상태 머신 | [`crates/mee-chain`](crates/mee-chain) | 서명 트랜잭션, 취소 우선 블록 순서, 브릿지 입출금, 거버넌스. 4코어 약 14만 tx/s |
| BFT 합의 | [`crates/mee-consensus`](crates/mee-consensus) | HotStuff 2-chain (Jolteon식), sans-IO, 장애 시뮬레이터 |
| 검증인 노드 | [`crates/mee-node`](crates/mee-node) | 합의 → 체인. 4노드 devnet 테스트 |
| 솔라나 프로그램 | [`programs/mee`](programs/mee) | 브릿지 (입금 기록, Ed25519 정족수 출금, 검증인 교체) + 온체인 HIP-3 DEX |
| 공용/온체인 엔진 | [`crates/mee-core`](crates/mee-core) | 공용 수학, 파라미터, 고정 크기 온체인 엔진 |
| 설계 문서 | [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) | 구조, 측정값, 프로토콜 규칙, 회계 공식, 한계 |

## 실행

```bash
cargo test --workspace
cargo bench -p mee-engine   # 엔진 처리량
cargo bench -p mee-chain    # 서명 검증 포함 체인 처리량
```

툴체인은 `rust-toolchain.toml`(1.97.0)에 고정돼 있다. 아직 프로토타입이고 감사 전이다. 남은 일은 [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md#12-한계와-다음-단계)에 정리했다.
