use hacp::secure::{
    session::SessionManager,
    transport::{ContractView, Transport},
    SecureError,
};

const ALICE: &str = "urn:hacp:agent:alice";
const BOB: &str = "urn:hacp:agent:bob";

#[test]
fn secure_payload_rejects_tampering_and_replay() {
    let mut alice = SessionManager::new(ALICE).expect("create Alice session manager");
    let mut bob = SessionManager::new(BOB).expect("create Bob session manager");
    let hello = alice
        .initiate(BOB, "hive-secure-smoke", &bob.public_key())
        .expect("initiate secure session");
    let acknowledgement = bob
        .respond("hive-secure-smoke", &hello, &alice.public_key())
        .expect("respond to secure session");
    let session_id = alice
        .complete("hive-secure-smoke", &acknowledgement)
        .expect("complete secure session");

    let mut sender = Transport::new();
    let mut receiver = Transport::new();
    let envelope = sender
        .seal(
            &mut alice,
            &session_id,
            "",
            b"payload from Hive",
            &ContractView::Bootstrap,
        )
        .expect("seal payload for peer");

    let mut tampered = envelope.clone();
    tampered.ct = Some("00".repeat(tampered.ct.as_ref().expect("ciphertext").len() / 2));
    let tamper_report = receiver.receive(&mut bob, &tampered, &ContractView::Bootstrap);
    assert!(tamper_report.delivered.is_empty());
    assert_eq!(
        tamper_report.rejected,
        vec![SecureError::BadMessageSignature]
    );

    let opened = receiver.receive(&mut bob, &envelope, &ContractView::Bootstrap);
    assert_eq!(opened.delivered.len(), 1);
    assert_eq!(opened.delivered[0].payload, b"payload from Hive");

    let replay = receiver.receive(&mut bob, &envelope, &ContractView::Bootstrap);
    assert!(replay.delivered.is_empty());
    assert_eq!(replay.rejected, vec![SecureError::ReplayRejected]);
}
