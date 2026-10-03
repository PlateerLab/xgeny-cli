'use strict';

function selectPlatform(catalog, platform, architecture) {
  if (!catalog || catalog.schemaVersion !== 1 || !Array.isArray(catalog.platforms)) {
    throw new Error('XGEN npm platform catalog is invalid');
  }

  const matches = catalog.platforms.filter(
    (candidate) => candidate.os === platform && candidate.cpu === architecture,
  );
  if (matches.length !== 1) {
    const error = new Error(`XGEN does not provide a native binary for ${platform}/${architecture}`);
    error.code = 'XGEN_UNSUPPORTED_PLATFORM';
    throw error;
  }
  return matches[0];
}

function resolvePlatformBinary(specification, resolver = require.resolve) {
  const request = `${specification.packageName}/${specification.binaryPath}`;
  try {
    return resolver(request);
  } catch (cause) {
    const error = new Error(
      `The required XGEN native package ${specification.packageName} is not installed. ` +
        'Reinstall @xgen/cli without --omit=optional.',
    );
    error.code = 'XGEN_PLATFORM_PACKAGE_MISSING';
    error.cause = cause;
    throw error;
  }
}

module.exports = { resolvePlatformBinary, selectPlatform };
