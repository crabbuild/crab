# ECS Fargate deployment

`task-definition.example.json` is an evaluation profile for running three or
more `crab-http-server` tasks behind an Application Load Balancer. Use the
Kubernetes chart for a team production candidate.

AWS Fargate limits a container stop timeout to 120 seconds. Crab permits Git and Large File Storage (LFS) transfers lasting five minutes and archive downloads lasting ten minutes. A task replacement can therefore terminate an active operation before Crab finishes its graceful drain. Abrupt-process-crash qualification must close this gap before the Fargate profile can carry a production-ready claim.

The profile assumes the cluster, virtual private cloud (VPC), private subnets, security groups, Application Load Balancer (ALB), target group, CloudWatch log group, Elastic Container Registry (ECR), Identity and Access Management (IAM) roles, bucket, and Secrets Manager values already exist. Those resources have organization-specific ownership and deletion policies, so this profile does not create them.

The configuration secret must use the task-local secret paths and one S3 root:

```toml
listen = "0.0.0.0:8788"
management_listen = "0.0.0.0:8789"

[cells]
data_dir = "/var/lib/crab/tmp/cells"
peer_advertise = "https://127.0.0.1:8789"
peer_tls_server_name = "crab-http-server-peer"
peer_certificate = "/var/lib/crab/tmp/peer.crt"
peer_private_key = "/var/lib/crab/tmp/peer.key"
peer_ca = "/var/lib/crab/tmp/peer-ca.crt"

[storage]
url = "s3://my-git-bucket/repositories"

[auth]
issuer = "https://identity.example/realm"
client_id = "crab-browser"
public_url = "https://git.example.com"
client_secret_file = "/var/lib/crab/tmp/oidc-client-secret"
state_key_file = "/var/lib/crab/tmp/state-key"
```

The peer leaf must use Ed25519, include `crab-http-server-peer` as a DNS SAN,
and be valid for both client and server authentication. Store the leaf, its
PKCS#8 private key, and the CA bundle in the three peer Secrets Manager values
named by the task definition. Use the same reviewed fleet trust set on every
task.

Grant the [ECS task role](https://docs.aws.amazon.com/AmazonECS/latest/developerguide/task-iam-roles.html)
bucket-list permission constrained to `repositories` and
object read/write/delete permission constrained to `repositories/*`. The
execution role needs ECR pull, CloudWatch log write, and read access to the
six named Secrets Manager values. Do not place AWS access keys in those
secrets; the server uses the Fargate task role credential chain.

Add an S3 lifecycle rule that expires objects below
`repositories/.crab/http-server/v1/auth/` after 24 hours. Do not apply that
rule to the catalog or repository prefixes.

Replace both the container image suffix and
`CRAB_HTTP_SERVER_RELEASE_IMAGE` with the same qualified manifest digest. The
task bootstraps or admits that exact compiled Cell release before starting the
listener; concurrent first tasks converge on one operation, and a different
pending release fails closed. At serving startup, the binary reads its preferred
`awsvpc` address from the task-local
[`ECS_CONTAINER_METADATA_URI_V4`](https://docs.aws.amazon.com/AmazonECS/latest/developerguide/ecs-environment-variables.html)
endpoint and replaces only the placeholder advertise host. It accepts no
operator-supplied metadata URL and sends no credentials with that request.
After replacing every placeholder:

```sh
aws ecs register-task-definition \
  --cli-input-json file://crates/crab-http-server/deploy/ecs/task-definition.example.json

aws ecs create-service \
  --cluster crab \
  --service-name crab-http-server \
  --task-definition crab-http-server \
  --desired-count 3 \
  --launch-type FARGATE \
  --deployment-configuration minimumHealthyPercent=100,maximumPercent=200 \
  --network-configuration 'awsvpcConfiguration={subnets=[subnet-a,subnet-b],securityGroups=[sg-task],assignPublicIp=DISABLED}' \
  --load-balancers targetGroupArn=arn:aws:elasticloadbalancing:REGION:ACCOUNT:targetgroup/NAME/ID,containerName=crab-http-server,containerPort=8788
```

Allow TCP 8789 only from the task security group to itself so owners are
directly reachable inside the VPC. Never expose that port on the ALB listener.
The container health check performs the authenticated `/readyz` request. An
ALB cannot present Crab's peer client certificate, so do not point an ALB probe
at the mTLS management listener. Configure the target group probe on traffic
port 8788 and path `/livez`, with the default `200` success matcher. `/livez`
only proves that the public process can answer; it accepts the task-private
[`Host` sent by ALB](https://docs.aws.amazon.com/elasticloadbalancing/latest/application/load-balancer-troubleshooting.html),
while the ECS container check remains the authoritative readiness signal.
Enable the ECS deployment circuit breaker and span tasks across availability
zones. Configure ALB idle timeouts, upstream request-body limits, and
deregistration delay for long Git and LFS streams. Those controls don’t extend
Fargate’s 120-second container stop limit.

This checked-in task definition is static deployment evidence, not live AWS
qualification. A release claim still requires a real push/fetch/LFS test,
forced task replacement, rolling upgrade, session callback across replicas,
Git-token use across replicas, and safe teardown against a dedicated bucket
prefix.
