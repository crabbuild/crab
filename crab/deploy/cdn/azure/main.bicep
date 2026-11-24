targetScope = 'resourceGroup'

@description('Region for the dedicated API Management service and storage account.')
param location string = resourceGroup().location

@description('Globally unique name for the dedicated classic Standard APIM service.')
param apimName string

@description('Publisher contact for the APIM service.')
param publisherEmail string

@description('Globally unique Front Door endpoint name.')
param endpointName string

@description('Globally unique Blob account name.')
param storageAccountName string

@description('Private container for the test repository.')
param containerName string

@description('Canonical logical repository prefix in the fixture container.')
@minLength(1)
@maxLength(255)
param repositoryPrefix string

@description('Exact suffix of one materialized v2 view, including its leading slash. Empty selects the repository-local profile.')
@maxLength(164)
param protectedViewSuffix string = ''

@description('Enable only after verifying APIM egress and the deployed WAF association.')
param enableViewerRoute bool = false

@sealed()
type ViewerSigningKey = {
  @minLength(1)
  @maxLength(63)
  id: string
  @minLength(342)
  @maxLength(342)
  modulus: string
  @minLength(1)
  @maxLength(16)
  exponent: string
}

@description('Public RSA keys as unpadded JWK base64url modulus and exponent. Multiple keys permit rollover.')
@minLength(1)
@maxLength(3)
param viewerSigningKeys ViewerSigningKey[]

var gatewayHost = '${apimName}.azure-api.net'
var blobHost = '${storageAccountName}.blob.${environment().suffixes.storage}'
var blobOrigin = 'https://${blobHost}/${containerName}'
var signingKeys = [
  for key in viewerSigningKeys: '<key id="${escapeXml(key.id)}" n="${escapeXml(key.modulus)}" e="${escapeXml(key.exponent)}" />'
]
var policyFields = {
  VIEWER_HOST: gatewayHost
  BLOB_ORIGIN: escapeXml(blobOrigin)
  REPOSITORY_PREFIX_BASE64: base64(repositoryPrefix)
  PROTECTED_VIEW_SUFFIX_BASE64: base64(protectedViewSuffix)
  SIGNING_KEYS: join(signingKeys, '\n')
  EDGE_HOST: endpoint.properties.hostName
}
var viewerPolicy = reduce(
  items(policyFields),
  loadTextContent('viewer-policy.xml'),
  (xml, field) => replace(xml, '__${field.key}__', field.value)
)

func escapeXml(value string) string => replace(replace(replace(replace(value, '&', '&amp;'), '"', '&quot;'), '<', '&lt;'), '>', '&gt;')

resource storage 'Microsoft.Storage/storageAccounts@2025-01-01' = {
  name: storageAccountName
  location: location
  sku: {
    name: 'Standard_LRS'
  }
  kind: 'StorageV2'
  properties: {
    isHnsEnabled: true
    minimumTlsVersion: 'TLS1_2'
    supportsHttpsTrafficOnly: true
    allowBlobPublicAccess: false
    allowSharedKeyAccess: false
    publicNetworkAccess: 'Enabled'
  }
}

resource blobs 'Microsoft.Storage/storageAccounts/blobServices@2025-01-01' = {
  parent: storage
  name: 'default'
}

resource container 'Microsoft.Storage/storageAccounts/blobServices/containers@2025-01-01' = {
  parent: blobs
  name: containerName
  properties: {
    publicAccess: 'None'
  }
}

resource frontDoor 'Microsoft.Cdn/profiles@2025-06-01' = {
  name: endpointName
  location: 'global'
  sku: {
    name: 'Premium_AzureFrontDoor'
  }
  identity: {
    type: 'SystemAssigned'
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

resource fillRole 'Microsoft.Authorization/roleAssignments@2022-04-01' = {
  scope: container
  name: guid(container.id, frontDoor.id, 'crab-fixture-blob-reader')
  properties: {
    principalId: frontDoor.identity.principalId
    principalType: 'ServicePrincipal'
    roleDefinitionId: subscriptionResourceId(
      'Microsoft.Authorization/roleDefinitions',
      '2a2b9908-6ea1-4ae2-8e65-a410df84e7d1'
    )
  }
}

resource originGroup 'Microsoft.Cdn/profiles/originGroups@2025-06-01' = {
  parent: frontDoor
  name: 'crab-blob'
  properties: {
    authentication: {
      type: 'SystemAssignedIdentity'
      scope: 'https://storage.azure.com/.default'
    }
    loadBalancingSettings: {
      additionalLatencyInMilliseconds: 0
      sampleSize: 4
      successfulSamplesRequired: 3
    }
    sessionAffinityState: 'Disabled'
  }
  // Attach the identity only after granting access to the private container.
  dependsOn: [
    fillRole
  ]
}

resource origin 'Microsoft.Cdn/profiles/originGroups/origins@2025-06-01' = {
  parent: originGroup
  name: 'crab-blob'
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

resource gateway 'Microsoft.ApiManagement/service@2024-05-01' = {
  name: apimName
  location: location
  sku: {
    name: 'Standard'
    capacity: 1
  }
  properties: {
    publisherName: 'Crab CDN qualification'
    publisherEmail: publisherEmail
    publicNetworkAccess: 'Enabled'
    virtualNetworkType: 'None'
  }
}

resource viewerApi 'Microsoft.ApiManagement/service/apis@2024-05-01' = {
  parent: gateway
  name: 'crab-viewer'
  properties: {
    displayName: 'Crab immutable object delivery'
    path: 'crab'
    protocols: [
      'https'
    ]
    subscriptionRequired: false
  }
}

resource viewerGet 'Microsoft.ApiManagement/service/apis/operations@2024-05-01' = {
  parent: viewerApi
  name: 'get-crab-object'
  properties: {
    displayName: 'Get immutable Crab object'
    method: 'GET'
    urlTemplate: '/*'
  }
}

resource apiPolicy 'Microsoft.ApiManagement/service/apis/policies@2024-05-01' = {
  parent: viewerApi
  name: 'policy'
  properties: {
    format: 'rawxml'
    value: viewerPolicy
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
          name: 'DenyOtherSockets'
          enabledState: 'Enabled'
          priority: 1
          ruleType: 'MatchRule'
          action: 'Block'
          matchConditions: [
            {
              matchVariable: 'SocketAddr'
              operator: 'IPMatch'
              negateCondition: true
              matchValue: gateway.properties.publicIPAddresses
            }
          ]
        }
      ]
    }
  }
}

resource edgeSecurity 'Microsoft.Cdn/profiles/securityPolicies@2025-06-01' = {
  parent: frontDoor
  name: 'crab-viewer'
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
          patternsToMatch: [
            '/*'
          ]
        }
      ]
    }
  }
}

resource route 'Microsoft.Cdn/profiles/afdEndpoints/routes@2025-06-01' = {
  parent: endpoint
  name: 'crab-immutable'
  properties: {
    enabledState: enableViewerRoute ? 'Enabled' : 'Disabled'
    linkToDefaultDomain: 'Enabled'
    httpsRedirect: 'Disabled'
    supportedProtocols: [
      'Https'
    ]
    forwardingProtocol: 'HttpsOnly'
    patternsToMatch: [
      '/*'
    ]
    originGroup: {
      id: originGroup.id
    }
    originPath: '/${container.name}'
    cacheConfiguration: {
      queryStringCachingBehavior: 'IgnoreQueryString'
      compressionSettings: {
        isCompressionEnabled: false
      }
    }
  }
  dependsOn: [
    origin
    edgeSecurity
    viewerGet
    apiPolicy
  ]
}

output originUrl string = blobOrigin
output repositoryPrefix string = repositoryPrefix
output physicalRepositoryPrefix string = '${repositoryPrefix}${protectedViewSuffix}'
output protectedGlobalPrefix string = empty(protectedViewSuffix) ? '' : '${repositoryPrefix}${protectedViewSuffix}/.crab'
output baseUrl string = 'https://${gatewayHost}/crab'
output frontDoorHost string = endpoint.properties.hostName
output apimPublicIPs array = gateway.properties.publicIPAddresses
