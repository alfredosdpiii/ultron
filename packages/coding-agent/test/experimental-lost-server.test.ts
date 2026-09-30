import { RemoteServiceError } from "@ultron/chord";
import { DisconnectedError } from "@ultron/client";
import { describe, expect, test } from "vitest";
import {
	clientErrorMessage,
	describeLostServer,
	friendlyStatus,
	isLostServerError,
} from "../src/experimental/lost-server.ts";

describe("lost server and worker messages", () => {
	test("the incident's bare binding error becomes a clear message with how to resume", () => {
		const error = new RemoteServiceError(
			"service_stale_instance",
			"Remote service pi.agent-controller binding is closed",
		);
		expect(describeLostServer(error)).toBe(
			"The Ultron server stopped (Remote service pi.agent-controller binding is closed); your session is saved — run `ultron -c` to resume",
		);
		expect(friendlyStatus("Error: Remote service pi.agent-controller binding is closed")).toBe(
			"Error: The Ultron server stopped (Remote service pi.agent-controller binding is closed); your session is saved — run `ultron -c` to resume",
		);
	});

	test("recognizes transport and worker loss, and leaves ordinary errors alone", () => {
		expect(isLostServerError(new DisconnectedError())).toBe(true);
		expect(isLostServerError(new Error("Byte transport closed"))).toBe(true);
		expect(isLostServerError(new Error("Session worker s-1 disconnected unexpectedly"))).toBe(true);
		expect(isLostServerError(new Error("Ultron server lost its coordinator during a worker operation"))).toBe(true);
		expect(isLostServerError(new Error("Model not found: test/none"))).toBe(false);
		expect(clientErrorMessage(new Error("Model not found: test/none"))).toBe("Model not found: test/none");
		expect(friendlyStatus("Error: Model not found: test/none")).toBe("Error: Model not found: test/none");
	});

	test("an already clear message is not wrapped twice", () => {
		const once = clientErrorMessage(new Error("Unix connection is closed"));
		expect(clientErrorMessage(new Error(once))).toBe(once);
		expect(friendlyStatus(`Error: ${once}`)).toBe(`Error: ${once}`);
	});
});
