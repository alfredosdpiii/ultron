/**
 * Test setup: a normal test run never reaches a real provider with the developer's own credentials.
 * Provider tests gated on an API key (`describe.skipIf(!process.env.X_API_KEY)`) are skipped because the keys
 * are removed from the environment before any test file loads. Set ULTRON_LIVE_TESTS=1 to run them on purpose.
 */
const CREDENTIAL =
	/(API_KEY|_TOKEN|_SECRET|ACCESS_KEY|ACCESS_KEY_ID|SECRET_ACCESS_KEY|SESSION_TOKEN|_AUTH|_CREDENTIALS)$/;
const PROVIDER_CONTEXT = [
	"AWS_PROFILE",
	"AWS_REGION",
	"AWS_DEFAULT_REGION",
	"GOOGLE_APPLICATION_CREDENTIALS",
	"GOOGLE_CLOUD_PROJECT",
	"GOOGLE_CLOUD_LOCATION",
	"AZURE_OPENAI_ENDPOINT",
	"AZURE_OPENAI_BASE_URL",
	"AZURE_OPENAI_RESOURCE_NAME",
	"CLOUDFLARE_ACCOUNT_ID",
];

if (process.env.ULTRON_LIVE_TESTS !== "1") {
	for (const name of Object.keys(process.env))
		if (CREDENTIAL.test(name) || PROVIDER_CONTEXT.includes(name)) delete process.env[name];
}
