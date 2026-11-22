targetScope = 'resourceGroup'

@description('Location of the new, dedicated export storage account.')
param location string = resourceGroup().location

@minLength(3)
@maxLength(24)
param storageAccountName string

@minLength(3)
@maxLength(63)
param containerName string

@description('Globally unique Front Door endpoint name.')
param endpointName string

@description('Canonical live Blob origin. The new export account must differ.')
param sourceOrigin string

@description('Canonical repo prefix. This limit keeps escaped WAF expressions within 256 characters.')
@minLength(1)
@maxLength(64)
param repositoryPrefix string

@description('SHA-256 of the exact reviewed public inventory.')
@minLength(64)
@maxLength(64)
param approvedInventorySha256 string

@description('Enable both public blob reads and the edge route after publishing the approved inventory.')
param enablePublicDelivery bool = false

var blobHost = '${storageAccountName}.blob.${environment().suffixes.storage}'
var blobOrigin = 'https://${blobHost}/${containerName}'
var escapedPrefix = replace(repositoryPrefix, '.', '\\.')
var shape = loadJsonContent('immutable-routes.json')
var validPrefixCharacters = allCharsIn(repositoryPrefix, 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789._-/')
var validPrefixSegments = empty(filter(split(repositoryPrefix, '/'), segment => contains(['', '.', '..'], segment)))
var sourceParts = split(sourceOrigin, '.blob.${environment().suffixes.storage}/')
var sourceAccount = replace(first(sourceParts), 'https://', '')
var sourceContainer = last(sourceParts)
var originChecks = [
  environment().name == 'AzureCloud'
  startsWith(sourceOrigin, 'https://')
  length(sourceParts) == 2
  length(sourceAccount) >= 3
  length(sourceAccount) <= 24
  allCharsIn(sourceAccount, 'abcdefghijklmnopqrstuvwxyz0123456789')
  length(sourceContainer) >= 3
  length(sourceContainer) <= 63
  allCharsIn(sourceContainer, 'abcdefghijklmnopqrstuvwxyz0123456789-')
  !startsWith(sourceContainer, '-')
  !endsWith(sourceContainer, '-')
  !contains(sourceContainer, '--')
  sourceAccount != storageAccountName
]
var validOrigin = empty(filter(originChecks, check => !check))
var active = enablePublicDelivery && validPrefixCharacters && validPrefixSegments && validOrigin && allCharsIn(approvedInventorySha256, '0123456789abcdef')

func allCharsIn(value string, chars string) bool => empty(filter(
  range(0, length(value)),
  index => !contains(chars, substring(value, index, 1))
))

var publicRequest = [
  {
    matchVariable: 'RequestMethod'
    operator: 'Equal'
    matchValue: ['GET']
  }
  {
    matchVariable: 'RequestHeader'
    selector: 'Host'
    operator: 'Equal'
    matchValue: [endpoint.properties.hostName]
  }
]

func allowImmutable(prefix string, path string, fanout array, conditions array) array => concat(
  conditions,
  [
    {
      matchVariable: 'RequestUri'
      operator: 'Regex'
      matchValue: [replace(path, '__REPOSITORY__', prefix)]
    }
  ],
  map(fanout, pattern => {
    matchVariable: 'RequestUri'
    operator: 'Regex'
    matchValue: [pattern]
  })
)

resource exportStorage 'Microsoft.Storage/storageAccounts@2025-01-01' = {
  name: storageAccountName
  location: location
  sku: {
    name: 'Standard_LRS'
  }
  kind: 'StorageV2'
  tags: {
    CrabPublicExport: 'true'
    CrabSourceOrigin: sourceOrigin
    CrabApprovedInventorySha256: approvedInventorySha256
  }
  properties: {
    isHnsEnabled: true
    minimumTlsVersion: 'TLS1_2'
    supportsHttpsTrafficOnly: true
    allowBlobPublicAccess: active
    allowSharedKeyAccess: false
    publicNetworkAccess: 'Enabled'
  }
}

resource blobs 'Microsoft.Storage/storageAccounts/blobServices@2025-01-01' = {
  parent: exportStorage
  name: 'default'
  properties: {
    deleteRetentionPolicy: {
      enabled: false
    }
    containerDeleteRetentionPolicy: {
      enabled: false
    }
  }
}

resource container 'Microsoft.Storage/storageAccounts/blobServices/containers@2025-01-01' = {
  parent: blobs
  name: containerName
  properties: {
    publicAccess: active ? 'Blob' : 'None'
  }
}

resource frontDoor 'Microsoft.Cdn/profiles@2025-06-01' = {
  name: endpointName
  location: 'global'
  sku: {
    name: 'Premium_AzureFrontDoor'
  }
  properties: {
    originResponseTimeoutSeconds: 120
  }
}

resource endpoint 'Microsoft.Cdn/profiles/afdEndpoints@2025-06-01' = {
  parent: frontDoor
  name: endpointName
  location: 'global'
  properties: {
    enabledState: 'Enabled'
  }
}

resource originGroup 'Microsoft.Cdn/profiles/originGroups@2025-06-01' = {
  parent: frontDoor
  name: 'crab-export-blob'
  properties: {
    loadBalancingSettings: {
      additionalLatencyInMilliseconds: 0
      sampleSize: 4
      successfulSamplesRequired: 3
    }
    sessionAffinityState: 'Disabled'
  }
}

resource origin 'Microsoft.Cdn/profiles/originGroups/origins@2025-06-01' = {
  parent: originGroup
  name: 'crab-export-blob'
  properties: {
    hostName: blobHost
    originHostHeader: blobHost
    httpPort: 80
    httpsPort: 443
    enforceCertificateNameCheck: true
    enabledState: 'Enabled'
    priority: 1
    weight: 1000
  }
}

resource waf 'Microsoft.Network/frontDoorWebApplicationFirewallPolicies@2025-03-01' = {
  name: 'Crab${uniqueString(resourceGroup().id, endpointName)}'
  location: 'global'
  sku: {
    name: 'Premium_AzureFrontDoor'
  }
  properties: {
    policySettings: {
      enabledState: 'Enabled'
      mode: 'Prevention'
    }
    customRules: {
      rules: [
        {
          name: 'DenyQueries'
          priority: 1
          enabledState: 'Enabled'
          ruleType: 'MatchRule'
          action: 'Block'
          matchConditions: [
            {
              matchVariable: 'QueryString'
              operator: 'GreaterThan'
              matchValue: ['0']
            }
          ]
        }
        {
          name: 'AllowGitExport'
          priority: 10
          enabledState: 'Enabled'
          ruleType: 'MatchRule'
          action: 'Allow'
          matchConditions: allowImmutable(escapedPrefix, shape.git.shape, shape.git.fanout, publicRequest)
        }
        {
          name: 'AllowLfsExport'
          priority: 11
          enabledState: 'Enabled'
          ruleType: 'MatchRule'
          action: 'Allow'
          matchConditions: allowImmutable(escapedPrefix, shape.lfs.shape, shape.lfs.fanout, publicRequest)
        }
        {
          name: 'DenyOtherRequests'
          priority: 100
          enabledState: 'Enabled'
          ruleType: 'MatchRule'
          action: 'Block'
          matchConditions: [
            {
              matchVariable: 'RequestUri'
              operator: 'Any'
              matchValue: []
            }
          ]
        }
      ]
    }
  }
}

resource edgeSecurity 'Microsoft.Cdn/profiles/securityPolicies@2025-06-01' = {
  parent: frontDoor
  name: 'crab-public-export'
  properties: {
    parameters: {
      type: 'WebApplicationFirewall'
      wafPolicy: {
        id: waf.id
      }
      associations: [
        {
          domains: [
            {
              id: endpoint.id
            }
          ]
          patternsToMatch: ['/*']
        }
      ]
    }
  }
}

resource route 'Microsoft.Cdn/profiles/afdEndpoints/routes@2025-06-01' = {
  parent: endpoint
  name: 'crab-immutable-export'
  properties: {
    enabledState: active ? 'Enabled' : 'Disabled'
    linkToDefaultDomain: 'Enabled'
    httpsRedirect: 'Disabled'
    supportedProtocols: ['Https']
    forwardingProtocol: 'HttpsOnly'
    patternsToMatch: ['/*']
    originGroup: {
      id: originGroup.id
    }
    originPath: '/${container.name}'
    cacheConfiguration: {
      queryStringCachingBehavior: 'UseQueryString'
      compressionSettings: {
        isCompressionEnabled: false
      }
    }
  }
  dependsOn: [
    origin
    edgeSecurity
  ]
}

output liveSourceOrigin string = sourceOrigin
output exportOrigin string = blobOrigin
output sourceRepositoryPrefix string = repositoryPrefix
output inventorySha256 string = approvedInventorySha256
output publicDeliveryEnabled bool = active
output baseUrl string = 'https://${endpoint.properties.hostName}'
output exportContainerId string = container.id
output edgeOriginId string = origin.id
output edgeRouteId string = route.id
