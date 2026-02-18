import type { ExtensionAPI } from "@mariozechner/pi-coding-agent";

async function runHook(
	pi: ExtensionAPI,
	hookName: string,
	extraArgs: string[] = [],
): Promise<string | null> {
	try {
		const result = await pi.exec("botbox", ["hooks", "run", hookName, ...extraArgs]);
		if (result.code !== 0) {
			return null;
		}

		const stdout = result.stdout?.trim();
		return stdout && stdout.length > 0 ? stdout : null;
	} catch {
		// Graceful degradation when botbox is not installed or hook execution fails.
		return null;
	}
}

function injectMessage(pi: ExtensionAPI, stdout: string) {
	pi.sendMessage({
		customType: "botbox-hook",
		content: stdout,
		display: false,
	});
}

export default function botboxHooksExtension(pi: ExtensionAPI) {
	let toolResultCount = 0;
	let pendingSessionStartContext = "";

	pi.on("session_start", async () => {
		const outputs: string[] = [];

		for (const hookName of ["init-agent", "check-jj", "claim-agent"]) {
			const stdout = await runHook(pi, hookName);
			if (stdout) {
				outputs.push(stdout);
			}
		}

		pendingSessionStartContext = outputs.join("\n\n");
	});

	pi.on("before_agent_start", async (event) => {
		if (!pendingSessionStartContext) {
			return;
		}

		const injected = pendingSessionStartContext;
		pendingSessionStartContext = "";

		return {
			systemPrompt: `${event.systemPrompt}\n\n${injected}`,
		};
	});

	pi.on("tool_result", async () => {
		toolResultCount += 1;
		if (toolResultCount % 5 !== 0) {
			return;
		}

		const stdout = await runHook(pi, "check-bus-inbox");
		if (stdout) {
			injectMessage(pi, stdout);
		}
	});

	pi.on("session_before_compact", async () => {
		const stdout = await runHook(pi, "init-agent");
		if (stdout) {
			injectMessage(pi, stdout);
		}
	});

	pi.on("session_shutdown", async () => {
		await runHook(pi, "claim-agent", ["--release"]);
	});
}
