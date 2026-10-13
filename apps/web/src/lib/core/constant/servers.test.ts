import { afterEach, describe, expect, it, vi } from 'vitest';
import { staticFileSizedUrl } from './servers';

describe('staticFileSizedUrl', () => {
  it.each([
    'data:image/png;base64,aW1hZ2U=',
    'blob:https://macro.com/avatar-id',
  ])('preserves embedded or local image sources: %s', (url) => {
    expect(staticFileSizedUrl(url, 'small')).toBe(url);
  });

  it('requests a thumbnail for uploaded images', () => {
    expect(
      staticFileSizedUrl('https://static.macro.com/file/avatar-id', 'small')
    ).toBe('https://static.macro.com/file/avatar-id?size=320');
  });
});

afterEach(() => {
  vi.unstubAllEnvs();
  vi.unstubAllGlobals();
  vi.resetModules();
});

it('uses a configured gateway origin in a production bundle', async () => {
  vi.stubEnv('MODE', 'production');
  vi.stubEnv('VITE_GATEWAY_ORIGIN', 'https://self-hosted.example/');
  vi.resetModules();
  const { SERVER_HOSTS } = await import('./servers');
  expect(SERVER_HOSTS['auth-service']).toBe('https://self-hosted.example/auth');
  expect(SERVER_HOSTS['document-storage-service']).toBe(
    'https://self-hosted.example/dss'
  );
  expect(SERVER_HOSTS['connection-gateway']).toBe(
    'wss://self-hosted.example/connection-gateway'
  );
});

it('uses a configured static-file base URL in a production bundle', async () => {
  vi.stubEnv('MODE', 'production');
  vi.stubEnv(
    'VITE_STATIC_FILE_PUBLIC_URL',
    'https://self-hosted.example/static-file/'
  );
  vi.resetModules();
  const { staticFileIdEndpoint, staticFileReferenceEndpoint } = await import(
    './servers'
  );
  expect(staticFileIdEndpoint('avatar-id')).toBe(
    'https://self-hosted.example/static-file/file/avatar-id'
  );
  expect(staticFileReferenceEndpoint('avatar-id')).toBe(
    'https://self-hosted.example/static-file/file/avatar-id'
  );
});

it('keeps browser image URLs proxied but gives the agent a fetchable hosted URL', async () => {
  vi.stubEnv('MODE', 'development');
  vi.stubEnv('VITE_DEV_PROXY', 'true');
  vi.stubEnv('VITE_LOCAL_SERVERS', '');
  vi.stubGlobal('location', { origin: 'https://localhost:3003' });
  vi.resetModules();
  const { staticFileIdEndpoint, staticFileReferenceEndpoint } = await import(
    './servers'
  );
  expect(staticFileIdEndpoint('test-image')).toBe(
    'https://localhost:3003/__macro_dev/static/file/test-image'
  );
  expect(staticFileReferenceEndpoint('test-image')).toBe(
    'https://static-file-service-dev.macro.com/file/test-image'
  );
});

it('preserves local-stack image references for the local attachment resolver', async () => {
  vi.stubEnv('MODE', 'development');
  vi.stubEnv('VITE_DEV_PROXY', 'true');
  vi.stubEnv('VITE_LOCAL_SERVERS', 'ALL');
  vi.stubEnv('VITE_LOCAL_BACKEND_ORIGIN', 'https://localhost:27709');
  vi.stubGlobal('location', new URL('https://localhost:3003'));
  vi.resetModules();
  const { staticFileReferenceEndpoint } = await import('./servers');
  expect(staticFileReferenceEndpoint('test-image')).toBe(
    'https://localhost:27709/static-file/file/test-image'
  );
});
