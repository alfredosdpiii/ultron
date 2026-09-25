import assert from "node:assert/strict";
import test from "node:test";
import { runPackedNativeCliAcceptance } from "./ultron-native-cli-consumer.mjs";

test("runs the packed Ultron native CLI against a deterministic local OpenAI-compatible provider", async () => {
	const result = await runPackedNativeCliAcceptance();
	assert.match(result.output, /ULTRON_PACKED_MOCK_OK/);
	assert.equal(result.requests.length, 1);
});
