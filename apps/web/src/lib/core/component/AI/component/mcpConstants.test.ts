import { afterEach, expect, it, vi } from 'vitest';

afterEach(() => {
  vi.unstubAllEnvs();
  vi.resetModules();
});

it('uses the configured MCP endpoint in every setup instruction', async () => {
  vi.stubEnv('VITE_MACRO_MCP_URL', 'https://self-hosted.example/mcp/');
  const { MACRO_MCP_URL, MACRO_MCP_CONFIG, CLI_COMMANDS } = await import(
    './mcpConstants'
  );

  expect(MACRO_MCP_URL).toBe('https://self-hosted.example/mcp');
  expect(JSON.parse(MACRO_MCP_CONFIG).mcpServers.macro.url).toBe(MACRO_MCP_URL);
  for (const { command } of CLI_COMMANDS) {
    expect(command).toContain(MACRO_MCP_URL);
  }
});
