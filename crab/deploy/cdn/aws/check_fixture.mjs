import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';

const template = readFileSync(new URL('./cloudfront.yaml', import.meta.url), 'utf8');
const publicTemplate = readFileSync(new URL('./public-export.yaml', import.meta.url), 'utf8');
const logTemplate = readFileSync(new URL('./qualification-logs.yaml', import.meta.url), 'utf8');

const repository = 'org/repo.v2';
const scope = 'a'.repeat(64);
const rootDigest = 'b'.repeat(64);
const view = `/acl-views/v2/${scope}/7-${rootDigest}`;
const hash = 'abcd' + 'e'.repeat(60);
const xetKinds = ['xorbs', 'shards', 'capsule-chunk-pages'];

for (const [configuration, suffix] of [[template, ''], [template, view], [publicTemplate, '']]) {
  const functionCode = configuration.match(/FunctionCode: !Sub \|\n((?: {8}[^\n]*\n)+)/)?.[1];
  assert.ok(functionCode, 'extract the deployed CloudFront viewer function');
  const handler = runInNewContext(
    `${functionCode.replaceAll('${RepositoryPrefix}', repository).replaceAll('${ProtectedViewSuffix}', suffix)}\nhandler`,
  );
  const prefix = `/${repository}${suffix}`;
  const capsule = `/v2/capsules/ab/${hash}`;
  const lfs = `/lfs/objects/ab/cd/${hash}`;
  const route = (uri, method = 'GET', querystring = {}) => {
    const request = { uri, method, querystring };
    return handler({ request }) === request;
  };

  assert.ok(route(prefix + capsule));
  assert.ok(route(prefix + lfs));
  assert.ok(route(`${prefix}/v2/history/source-roots/ab/${hash}`));

  for (const kind of xetKinds) {
    const object = `/.crab/${kind}/ab/${hash}`;
    assert.equal(route(prefix + object), Boolean(suffix));
    assert.equal(route(`/${repository}${view.replace('/7-', '/8-')}${object}`), false);
    assert.equal(route(`/${repository}${view.replace(scope, 'c'.repeat(64))}${object}`), false);
    assert.equal(route(object), false);
    assert.equal(route(`${prefix}/.crab/${kind}/aa/${hash}`), false);
    assert.equal(route(`${prefix}/.crab/${kind}/%61b/${hash}`), false);
    assert.equal(route(`${prefix}${object}\n`), false);
    assert.equal(route(prefix + object, 'HEAD'), false);
    assert.equal(route(prefix + object, 'GET', { versionId: { value: 'fixture' } }), false);
  }
  assert.equal(route(prefix + '/v2/root'), false);
  assert.equal(route(prefix + capsule, 'GET', { versionId: { value: 'fixture' } }), false);
  assert.equal(route(prefix + lfs, 'GET', { 'response-content-type': { value: 'text/html' } }), false);
  assert.equal(route(prefix + capsule, 'HEAD'), false);
  assert.equal(route(prefix + '/'), false);
  assert.equal(route(capsule), false);
  assert.equal(route(prefix + capsule + '/sibling'), false);
  assert.equal(route(`/${repository}${suffix ? '' : view}${capsule}`), false);
}

const originResources = [...template.matchAll(/^ +-(?: !Sub '| !If \[ProtectedView, !Sub ')\$\{Origin\.Arn\}\/([^']+)'(.*)$/gm)];
assert.equal(originResources.length, 9);
for (const [, path] of originResources) {
  assert.ok(path.startsWith('${RepositoryPrefix}${ProtectedViewSuffix}/'));
}
for (const kind of xetKinds) {
  const rule = originResources.find(([, path]) => path.endsWith(`/.crab/${kind}/*`));
  assert.ok(rule?.[0].includes('!If [ProtectedView'));
  assert.ok(rule[2].includes("!Ref 'AWS::NoValue'"));
}

assert.ok(template.includes('Assert: !Not [!Equals [!Ref ViewerPublicKey, !Ref SecondaryViewerPublicKey]]'));
assert.ok(template.includes("!If [PrimaryViewerKey, !Ref ViewerKey, !Ref 'AWS::NoValue']"));
assert.ok(template.includes("!If [SecondaryViewerKey, !Ref ViewerKeySecondary, !Ref 'AWS::NoValue']"));
assert.ok(template.includes('TrustedKeyGroups: [!Ref ViewerKeyGroup]'));
assert.match(publicTemplate, /Enabled: !If \[PublicDelivery, true, false\]/);
assert.match(publicTemplate, /EnablePublicDelivery:\n    Type: String\n    Default: 'false'/);
assert.match(publicTemplate, /DomainName: !GetAtt ExportBucket\.RegionalDomainName/);
assert.match(publicTemplate, /Bucket: !Ref ExportBucket/);
assert.match(publicTemplate, /AWS:SourceArn: !Sub 'arn:\$\{AWS::Partition\}:cloudfront::\$\{AWS::AccountId\}:distribution\/\$\{Distribution\}'/);
assert.match(publicTemplate, /aws:PrincipalArn: !Ref ExportWriterRoleArn/);
assert.match(publicTemplate, /s3:ObjectCreationOperation: 'true'\n              StringNotEquals:\n                s3:if-none-match: '\*'/);
assert.match(publicTemplate, /Prefix: !Sub '\$\{RepositoryPrefix\}\/'\n            AbortIncompleteMultipartUpload:\n              DaysAfterInitiation: 7/);
assert.doesNotMatch(publicTemplate, /TrustedKeyGroups|Action: s3:ListBucket|DomainName:.*SourceOrigin/);

const viewerFields = logTemplate.match(/RecordFields:\n((?: {8}- [^\n]+\n)+)/)?.[1];
assert.ok(viewerFields, 'CloudFront delivery selects the measured fields explicitly');
assert.deepEqual(viewerFields.trim().split('\n').map((line) => line.trim().slice(2)), [
  'date', 'time', 'x-edge-request-id', 'x-host-header', 'cs-method', 'cs-uri-stem',
  'cs-protocol', 'sc-status', 'sc-bytes', 'x-edge-response-result-type',
  'sc-content-len', 'sc-range-start', 'sc-range-end',
]);
assert.match(logTemplate, /OutputFormat: w3c/);
assert.match(logTemplate, /FieldDelimiter: "\\t"/);
assert.match(logTemplate, /DestinationResourceArn: !Sub '\$\{LogBucket\.Arn\}\/AWSLogs\/\$\{AWS::AccountId\}\/CloudFront'/);
assert.match(logTemplate, /S3EnableHiveCompatiblePath: false/);
assert.match(logTemplate, /S3SuffixPath: '\{yyyy\}\/\{MM\}\/\{dd\}\/\{HH\}\/'/);
assert.match(logTemplate, /BucketOwnerEnforced/);
assert.match(logTemplate, /BlockPublicPolicy: true/);
assert.match(logTemplate, /IgnorePublicAcls: true/);
assert.match(logTemplate, /RestrictPublicBuckets: true/);
assert.match(logTemplate, /BlockPublicAcls: true/);
assert.match(logTemplate, /ExpirationInDays: 14/);
assert.match(logTemplate, /DependsOn: LogBucketPolicy/);
assert.match(logTemplate, /aws:SourceArn: !GetAtt ViewerSource\.Arn/);
assert.match(logTemplate, /Resource: !Sub '\$\{LogBucket\.Arn\}\/AWSLogs\/\$\{AWS::AccountId\}\/CloudFront\/\*'/);
assert.match(logTemplate, /Service: logging\.s3\.amazonaws\.com[\s\S]+aws:SourceArn: !Sub 'arn:\$\{AWS::Partition\}:s3:::\$\{OriginBucketName\}'/);
