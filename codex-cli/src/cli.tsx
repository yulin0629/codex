#!/usr/bin/env node
import "dotenv/config";

// Exit early if on an older version of Node.js (< 22)
const major = process.versions.node.split(".").map(Number)[0]!;
if (major < 22) {
  // eslint-disable-next-line no-console
  console.error(
    "\n" +
      "Codex CLI requires Node.js version 22 or newer.\n" +
      `You are running Node.js v${process.versions.node}.\n` +
      "Please upgrade Node.js: https://nodejs.org/en/download/\n",
  );
  process.exit(1);
}

// Hack to suppress deprecation warnings (punycode)
// eslint-disable-next-line @typescript-eslint/no-explicit-any
(process as any).noDeprecation = true;

import type { AppRollout } from "./app";
import type { ApprovalPolicy } from "./approvals";
import type { CommandConfirmation } from "./utils/agent/agent-loop";
import type { AppConfig } from "./utils/config";
import type { ResponseItem } from "openai/resources/responses/responses";
import type { ReasoningEffort } from "openai/resources.mjs";

import App from "./app";
import { runSinglePass } from "./cli-singlepass";
import SessionsOverlay from "./components/sessions-overlay.js";
import { AgentLoop } from "./utils/agent/agent-loop";
import { ReviewDecision } from "./utils/agent/review";
import { AutoApprovalMode } from "./utils/auto-approval-mode";
import { checkForUpdates } from "./utils/check-updates";
import {
  loadConfig,
  PRETTY_PRINT,
  INSTRUCTIONS_FILEPATH,
} from "./utils/config";
import {
  getApiKey as fetchApiKey,
  maybeRedeemCredits,
} from "./utils/get-api-key";
import { createInputItem } from "./utils/input-utils";
import { initLogger } from "./utils/logger/log";
import { isModelSupportedForResponses } from "./utils/model-utils.js";
import { parseToolCall } from "./utils/parsers";
import { providers } from "./utils/providers";
import { onExit, setInkRenderer } from "./utils/terminal";
import chalk from "chalk";
import { spawnSync } from "child_process";
import fs from "fs";
import { render } from "ink";
import meow from "meow";
import os from "os";
import path from "path";
import React from "react";

// Call this early so `tail -F "$TMPDIR/oai-codex/codex-cli-latest.log"` works
// immediately. This must be run with DEBUG=1 for logging to work.
initLogger();

// TODO: migrate to new versions of quiet mode
//
//     -q, --quiet    Non-interactive quiet mode that only prints final message
//     -j, --json     Non-interactive JSON output mode that prints JSON messages

const cli = meow(
  `
  Usage
    $ codex [options] <prompt>
    $ codex completion <bash|zsh|fish>

  Options
    --version                       Print version and exit

    -h, --help                      Show usage and exit
    -m, --model <model>             Model to use for completions (default: codex-mini-latest)
    -p, --provider <provider>       Provider to use for completions (default: openai)
    -i, --image <path>              Path(s) to image files to include as input
    -v, --view <rollout>            Inspect a previously saved rollout instead of starting a session
    --history                       Browse previous sessions
    --login                         Start a new sign in flow
    --free                          Retry redeeming free credits
    -q, --quiet                     Non-interactive mode that only prints the assistant's final output
    -c, --config                    Open the instructions file in your editor
    -w, --writable-root <path>      Writable folder for sandbox in full-auto mode (can be specified multiple times)
    -a, --approval-mode <mode>      Override the approval policy: 'suggest', 'auto-edit', or 'full-auto'

    --auto-edit                Automatically approve file edits; still prompt for commands
    --full-auto                Automatically approve edits and commands when executed in the sandbox

    --no-project-doc           Do not automatically include the repository's 'AGENTS.md'
    --project-doc <file>       Include an additional markdown file at <file> as context
    --full-stdout              Do not truncate stdout/stderr from command outputs
    --notify                   Enable desktop notifications for responses

    --disable-response-storage Disable server‑side response storage (sends the
                               full conversation context with every request)

    --flex-mode               Use "flex-mode" processing mode for the request (only supported
                              with models o3 and o4-mini)

    --reasoning <effort>      Set the reasoning effort level (low, medium, high) (default: high)

  Dangerous options
    --dangerously-auto-approve-everything
                               Skip all confirmation prompts and execute commands without
                               sandboxing. Intended solely for ephemeral local testing.

  Experimental options
    -f, --full-context         Launch in "full-context" mode which loads the entire repository
                               into context and applies a batch of edits in one go. Incompatible
                               with all other flags, except for --model.

  Examples
    $ codex "Write and run a python program that prints ASCII art"
    $ codex -q "fix build issues"
    $ codex completion bash
`,
  {
    importMeta: import.meta,
    autoHelp: true,
    flags: {
      // misc
      help: { type: "boolean", aliases: ["h"] },
      version: { type: "boolean", description: "Print version and exit" },
      view: { type: "string" },
      history: { type: "boolean", description: "Browse previous sessions" },
      login: { type: "boolean", description: "Force a new sign in flow" },
      free: { type: "boolean", description: "Retry redeeming free credits" },
      model: { type: "string", aliases: ["m"] },
      provider: { type: "string", aliases: ["p"] },
      image: { type: "string", isMultiple: true, aliases: ["i"] },
      quiet: {
        type: "boolean",
        aliases: ["q"],
        description: "Non-interactive quiet mode",
      },
      config: {
        type: "boolean",
        aliases: ["c"],
        description: "Open the instructions file in your editor",
      },
      dangerouslyAutoApproveEverything: {
        type: "boolean",
        description:
          "Automatically approve all commands without prompting. This is EXTREMELY DANGEROUS and should only be used in trusted environments.",
      },
      autoEdit: {
        type: "boolean",
        description: "Automatically approve edits; prompt for commands.",
      },
      fullAuto: {
        type: "boolean",
        description:
          "Automatically run commands in a sandbox; only prompt for failures.",
      },
      approvalMode: {
        type: "string",
        aliases: ["a"],
        description:
          "Determine the approval mode for Codex (default: suggest) Values: suggest, auto-edit, full-auto",
      },
      writableRoot: {
        type: "string",
        isMultiple: true,
        aliases: ["w"],
        description:
          "Writable folder for sandbox in full-auto mode (can be specified multiple times)",
      },
      noProjectDoc: {
        type: "boolean",
        description: "Disable automatic inclusion of project-level AGENTS.md",
      },
      projectDoc: {
        type: "string",
        description: "Path to a markdown file to include as project doc",
      },
      flexMode: {
        type: "boolean",
        description:
          "Enable the flex-mode service tier (only supported by models o3 and o4-mini)",
      },
      fullStdout: {
        type: "boolean",
        description:
          "Disable truncation of command stdout/stderr messages (show everything)",
        aliases: ["no-truncate"],
      },
      reasoning: {
        type: "string",
        description: "Set the reasoning effort level (low, medium, high)",
        choices: ["low", "medium", "high"],
        default: "high",
      },
      // Notification
      notify: {
        type: "boolean",
        description: "Enable desktop notifications for responses",
      },

      disableResponseStorage: {
        type: "boolean",
        description:
          "Disable server-side response storage (sends full conversation context with every request)",
      },

      // Experimental mode where whole directory is loaded in context and model is requested
      // to make code edits in a single pass.
      fullContext: {
        type: "boolean",
        aliases: ["f"],
        description: `Run in full-context editing approach. The model is given the whole code
          directory as context and performs changes in one go without acting.`,
      },
    },
  },
);

// ---------------------------------------------------------------------------
// Global flag handling
// ---------------------------------------------------------------------------

// Handle 'completion' subcommand before any prompting or API calls
if (cli.input[0] === "completion") {
  const shell = cli.input[1] || "bash";
  const scripts: Record<string, string> = {
    bash: `# bash completion for codex
_codex_completion() {
  local cur
  cur="\${COMP_WORDS[COMP_CWORD]}"
  COMPREPLY=( $(compgen -o default -o filenames -- "\${cur}") )
}
complete -F _codex_completion codex`,
    zsh: `# zsh completion for codex
#compdef codex

_codex() {
  _arguments '*:filename:_files'
}
_codex`,
    fish: `# fish completion for codex
complete -c codex -a '(__fish_complete_path)' -d 'file path'`,
  };
  const script = scripts[shell];
  if (!script) {
    // eslint-disable-next-line no-console
    console.error(`Unsupported shell: ${shell}`);
    process.exit(1);
  }
  // eslint-disable-next-line no-console
  console.log(script);
  process.exit(0);
}

// For --help, show help and exit.
if (cli.flags.help) {
  cli.showHelp();
}

// For --config, open custom instructions file in editor and exit.
if (cli.flags.config) {
  try {
    loadConfig(); // Ensures the file is created if it doesn't already exit.
  } catch {
    // ignore errors
  }

  const filePath = INSTRUCTIONS_FILEPATH;
  const editor =
    process.env["EDITOR"] || (process.platform === "win32" ? "notepad" : "vi");
  spawnSync(editor, [filePath], { stdio: "inherit" });
  process.exit(0);
}

// ---------------------------------------------------------------------------
// API key handling
// ---------------------------------------------------------------------------

const fullContextMode = Boolean(cli.flags.fullContext);
let config = loadConfig(undefined, undefined, {
  cwd: process.cwd(),
  disableProjectDoc: Boolean(cli.flags.noProjectDoc),
  projectDocPath: cli.flags.projectDoc,
  isFullContext: fullContextMode,
});

// `prompt` can be updated later when the user resumes a previous session
// via the `--history` flag. Therefore it must be declared with `let` rather
// than `const`.
let prompt = cli.input[0];
const model = cli.flags.model ?? config.model;
const imagePaths = cli.flags.image;
const provider = cli.flags.provider ?? config.provider ?? "openai";

const client = {
  issuer: "https://auth.openai.com",
  client_id: "app_EMoamEEZ73f0CkXaXp7hrann",
};

let apiKey = "";
let savedTokens:
  | {
      id_token?: string;
      access_token?: string;
      refresh_token: string;
    }
  | undefined;

// Try to load existing auth file if present
try {
  const home = os.homedir();
  const authDir = path.join(home, ".codex");
  const authFile = path.join(authDir, "auth.json");
  if (fs.existsSync(authFile)) {
    const data = JSON.parse(fs.readFileSync(authFile, "utf-8"));
    savedTokens = data.tokens;
    const lastRefreshTime = data.last_refresh
      ? new Date(data.last_refresh).getTime()
      : 0;
    const expired = Date.now() - lastRefreshTime > 28 * 24 * 60 * 60 * 1000;
    if (data.OPENAI_API_KEY && !expired) {
      apiKey = data.OPENAI_API_KEY;
    }
  }
} catch {
  // ignore errors
}

// Get provider-specific API key if not OpenAI
if (provider.toLowerCase() !== "openai") {
  const providerInfo = providers[provider.toLowerCase()];
  if (providerInfo) {
    const providerApiKey = process.env[providerInfo.envKey];
    if (providerApiKey) {
      apiKey = providerApiKey;
    }
  }
}

// Only proceed with OpenAI auth flow if:
// 1. Provider is OpenAI and no API key is set, or
// 2. Login flag is explicitly set
if (provider.toLowerCase() === "openai" && !apiKey) {
  if (cli.flags.login) {
    apiKey = await fetchApiKey(client.issuer, client.client_id);
    try {
      const home = os.homedir();
      const authDir = path.join(home, ".codex");
      const authFile = path.join(authDir, "auth.json");
      if (fs.existsSync(authFile)) {
        const data = JSON.parse(fs.readFileSync(authFile, "utf-8"));
        savedTokens = data.tokens;
      }
    } catch {
      /* ignore */
    }
  } else {
    apiKey = await fetchApiKey(client.issuer, client.client_id);
  }
}

// Ensure the API key is available as an environment variable for legacy code
process.env["OPENAI_API_KEY"] = apiKey;

// Only attempt credit redemption for OpenAI provider
if (cli.flags.free && provider.toLowerCase() === "openai") {
  // eslint-disable-next-line no-console
  console.log(`${chalk.bold("codex --free")} attempting to redeem credits...`);
  if (!savedTokens?.refresh_token) {
    apiKey = await fetchApiKey(client.issuer, client.client_id, true);
    // fetchApiKey includes credit redemption as the end of the flow
  } else {
    await maybeRedeemCredits(
      client.issuer,
      client.client_id,
      savedTokens.refresh_token,
      savedTokens.id_token,
    );
  }
}

// Set of providers that don't require API keys
const NO_API_KEY_REQUIRED = new Set(["ollama"]);

// Skip API key validation for providers that don't require an API key
if (!apiKey && !NO_API_KEY_REQUIRED.has(provider.toLowerCase())) {
  // eslint-disable-next-line no-console
  console.error(
    `\n${chalk.red(`Missing ${provider} API key.`)}\n\n` +
      `Set the environment variable ${chalk.bold(
        `${provider.toUpperCase()}_API_KEY`,
      )} ` +
      `and re-run this command.\n` +
      `${
        provider.toLowerCase() === "openai"
          ? `You can create a key here: ${chalk.bold(
              chalk.underline("https://platform.openai.com/account/api-keys"),
            )}\n`
          : provider.toLowerCase() === "azure"
            ? `You can create a ${chalk.bold(
                `${provider.toUpperCase()}_OPENAI_API_KEY`,
              )} ` +
              `in Azure AI Foundry portal at ${chalk.bold(chalk.underline("https://ai.azure.com"))}.\n`
            : provider.toLowerCase() === "gemini"
              ? `You can create a ${chalk.bold(
                  `${provider.toUpperCase()}_API_KEY`,
                )} ` + `in the ${chalk.bold(`Google AI Studio`)}.\n`
              : `You can create a ${chalk.bold(
                  `${provider.toUpperCase()}_API_KEY`,
                )} ` + `in the ${chalk.bold(`${provider}`)} dashboard.\n`
      }`,
  );
  process.exit(1);
}

const flagPresent = Object.hasOwn(cli.flags, "disableResponseStorage");

const disableResponseStorage = flagPresent
  ? Boolean(cli.flags.disableResponseStorage) // value user actually passed
  : (config.disableResponseStorage ?? false); // fall back to YAML, default to false

config = {
  apiKey,
  ...config,
  model: model ?? config.model,
  notify: Boolean(cli.flags.notify),
  reasoningEffort:
    (cli.flags.reasoning as ReasoningEffort | undefined) ?? "medium",
  flexMode: cli.flags.flexMode || (config.flexMode ?? false),
  provider,
  disableResponseStorage,
};

// Check for updates after loading config. This is important because we write state file in
// the config dir.
try {
  await checkForUpdates();
} catch {
  // ignore
}

// For --flex-mode, validate and exit if incorrect.
if (config.flexMode) {
  const allowedFlexModels = new Set(["o3", "o4-mini"]);
  if (!allowedFlexModels.has(config.model)) {
    if (cli.flags.flexMode) {
      // eslint-disable-next-line no-console
      console.error(
        `The --flex-mode option is only supported when using the 'o3' or 'o4-mini' models. ` +
          `Current model: '${config.model}'.`,
      );
      process.exit(1);
    } else {
      config.flexMode = false;
    }
  }
}

if (
  !(await isModelSupportedForResponses(provider, config.model)) &&
  (!provider || provider.toLowerCase() === "openai")
) {
  // eslint-disable-next-line no-console
  console.error(
    `The model "${config.model}" does not appear in the list of models ` +
      `available to your account. Double-check the spelling (use\n` +
      `  openai models list\n` +
      `to see the full list) or choose another model with the --model flag.`,
  );
  process.exit(1);
}

let rollout: AppRollout | undefined;

// For --history, show session selector and optionally update prompt or rollout.
if (cli.flags.history) {
  const result: { path: string; mode: "view" | "resume" } | null =
    await new Promise((resolve) => {
      const instance = render(
        React.createElement(SessionsOverlay, {
          onView: (p: string) => {
            instance.unmount();
            resolve({ path: p, mode: "view" });
          },
          onResume: (p: string) => {
            instance.unmount();
            resolve({ path: p, mode: "resume" });
          },
          onExit: () => {
            instance.unmount();
            resolve(null);
          },
        }),
      );
    });

  if (!result) {
    process.exit(0);
  }

  if (result.mode === "view") {
    try {
      const content = fs.readFileSync(result.path, "utf-8");
      rollout = JSON.parse(content) as AppRollout;
    } catch (error) {
      // eslint-disable-next-line no-console
      console.error("Error reading session file:", error);
      process.exit(1);
    }
  } else {
    prompt = `Resume this session: ${result.path}`;
  }
}

// For --view, optionally load an existing rollout from disk, display it and exit.
if (cli.flags.view) {
  const viewPath = cli.flags.view;
  const absolutePath = path.isAbsolute(viewPath)
    ? viewPath
    : path.join(process.cwd(), viewPath);
  try {
    const content = fs.readFileSync(absolutePath, "utf-8");
    rollout = JSON.parse(content) as AppRollout;
  } catch (error) {
    // eslint-disable-next-line no-console
    console.error("Error reading rollout file:", error);
    process.exit(1);
  }
}

// For --fullcontext, run the separate cli entrypoint and exit.
if (fullContextMode) {
  await runSinglePass({
    originalPrompt: prompt,
    config,
    rootPath: process.cwd(),
  });
  onExit();
  process.exit(0);
}

// Ensure that all values in additionalWritableRoots are absolute paths.
const additionalWritableRoots: ReadonlyArray<string> = (
  cli.flags.writableRoot ?? []
).map((p) => path.resolve(p));

// For --quiet, run the cli without user interactions and exit.
if (cli.flags.quiet) {
  process.env["CODEX_QUIET_MODE"] = "1";
  if (!prompt || prompt.trim() === "") {
    // eslint-disable-next-line no-console
    console.error(
      'Quiet mode requires a prompt string, e.g.,: codex -q "Fix bug #123 in the foobar project"',
    );
    process.exit(1);
  }

  // Determine approval policy for quiet mode based on flags
  const quietApprovalPolicy: ApprovalPolicy =
    cli.flags.fullAuto || cli.flags.approvalMode === "full-auto"
      ? AutoApprovalMode.FULL_AUTO
      : cli.flags.autoEdit || cli.flags.approvalMode === "auto-edit"
        ? AutoApprovalMode.AUTO_EDIT
        : config.approvalMode || AutoApprovalMode.SUGGEST;

  await runQuietMode({
    prompt,
    imagePaths: imagePaths || [],
    approvalPolicy: quietApprovalPolicy,
    additionalWritableRoots,
    config,
  });
  onExit();
  process.exit(0);
}

// Default to the "suggest" policy.
// Determine the approval policy to use in interactive mode.
//
// Priority (highest → lowest):
// 1. --fullAuto – run everything automatically in a sandbox.
// 2. --dangerouslyAutoApproveEverything – run everything **without** a sandbox
//    or prompts.  This is intended for completely trusted environments.  Since
//    it is more dangerous than --fullAuto we deliberately give it lower
//    priority so a user specifying both flags still gets the safer behaviour.
// 3. --autoEdit – automatically approve edits, but prompt for commands.
// 4. config.approvalMode - use the approvalMode setting from ~/.codex/config.json.
// 5. Default – suggest mode (prompt for everything).

const approvalPolicy: ApprovalPolicy =
  cli.flags.fullAuto || cli.flags.approvalMode === "full-auto"
    ? AutoApprovalMode.FULL_AUTO
    : cli.flags.autoEdit || cli.flags.approvalMode === "auto-edit"
      ? AutoApprovalMode.AUTO_EDIT
      : config.approvalMode || AutoApprovalMode.SUGGEST;

const instance = render(
  <App
    prompt={prompt}
    config={config}
    rollout={rollout}
    imagePaths={imagePaths}
    approvalPolicy={approvalPolicy}
    additionalWritableRoots={additionalWritableRoots}
    fullStdout={Boolean(cli.flags.fullStdout)}
  />,
  {
    patchConsole: process.env["DEBUG"] ? false : true,
  },
);
setInkRenderer(instance);

function formatResponseItemForQuietMode(item: ResponseItem): string {
  if (!PRETTY_PRINT) {
    return JSON.stringify(item);
  }
  switch (item.type) {
    case "message": {
      const role = item.role === "assistant" ? "assistant" : item.role;
      const txt = item.content
        .map((c) => {
          if (c.type === "output_text" || c.type === "input_text") {
            return c.text;
          }
          if (c.type === "input_image") {
            return "<Image>";
          }
          if (c.type === "input_file") {
            return c.filename;
          }
          if (c.type === "refusal") {
            return c.refusal;
          }
          return "?";
        })
        .join(" ");
      return `${role}: ${txt}`;
    }
    case "function_call": {
      const details = parseToolCall(item);
      return `$ ${details?.cmdReadableText ?? item.name}`;
    }
    case "function_call_output": {
      // @ts-expect-error metadata unknown on ResponseFunctionToolCallOutputItem
      const meta = item.metadata as ExecOutputMetadata;
      const parts: Array<string> = [];
      if (typeof meta?.exit_code === "number") {
        parts.push(`code: ${meta.exit_code}`);
      }
      if (typeof meta?.duration_seconds === "number") {
        parts.push(`duration: ${meta.duration_seconds}s`);
      }
      const header = parts.length > 0 ? ` (${parts.join(", ")})` : "";
      return `command.stdout${header}\n${item.output}`;
    }
    default: {
      return JSON.stringify(item);
    }
  }
}

async function runQuietMode({
  prompt,
  imagePaths,
  approvalPolicy,
  additionalWritableRoots,
  config,
}: {
  prompt: string;
  imagePaths: Array<string>;
  approvalPolicy: ApprovalPolicy;
  additionalWritableRoots: ReadonlyArray<string>;
  config: AppConfig;
}): Promise<void> {
  const agent = new AgentLoop({
    model: config.model,
    config: config,
    instructions: config.instructions,
    provider: config.provider,
    approvalPolicy,
    additionalWritableRoots,
    disableResponseStorage: config.disableResponseStorage,
    onItem: (item: ResponseItem) => {
      // eslint-disable-next-line no-console
      console.log(formatResponseItemForQuietMode(item));
    },
    onLoading: () => {
      /* intentionally ignored in quiet mode */
    },
    getCommandConfirmation: (
      _command: Array<string>,
    ): Promise<CommandConfirmation> => {
      // In quiet mode, default to NO_CONTINUE, except when in full-auto mode
      const reviewDecision =
        approvalPolicy === AutoApprovalMode.FULL_AUTO
          ? ReviewDecision.YES
          : ReviewDecision.NO_CONTINUE;
      return Promise.resolve({ review: reviewDecision });
    },
    onLastResponseId: () => {
      /* intentionally ignored in quiet mode */
    },
  });

  const inputItem = await createInputItem(prompt, imagePaths);
  await agent.run([inputItem]);
}

const exit = () => {
  onExit();
  process.exit(0);
};

process.on("SIGINT", exit);
process.on("SIGQUIT", exit);
process.on("SIGTERM", exit);

// ---------------------------------------------------------------------------
// Fallback for Ctrl-C when stdin is in raw-mode
// ---------------------------------------------------------------------------

if (process.stdin.isTTY) {
  // Ensure we do not leave the terminal in raw mode if the user presses
  // Ctrl-C while some other component has focus and Ink is intercepting
  // input. Node does *not* emit a SIGINT in raw-mode, so we listen for the
  // corresponding byte (0x03) ourselves and trigger a graceful shutdown.
  const onRawData = (data: Buffer | string): void => {
    const str = Buffer.isBuffer(data) ? data.toString("utf8") : data;
    if (str === "\u0003") {
      exit();
    }
  };
  process.stdin.on("data", onRawData);
}

// Ensure terminal clean-up always runs, even when other code calls
// `process.exit()` directly.
process.once("exit", onExit);
