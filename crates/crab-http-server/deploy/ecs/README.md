# ECS Fargate deployment

`task-definition.example.json` is a hardened starting point for running two or
more `crab-http-server` tasks behind an Application Load Balancer. It assumes
the cluster, VPC, private subnets, security groups, ALB, target group, CloudWatch
log group, ECR repository, IAM roles, bucket, and Secrets Manager values already
exist. Those resources have organization-specific ownership and deletion
policies, so this profile does not create them.

The configuration secret must use the task-local secret paths and one S3 root:

```toml
listen = "0.0.0.0:8788"
management_listen = "0.0.0.0:8789"

[storage]
url = "s3://my-git-bucket/repositories"

[auth]
issuer = "https://identity.example/realm"
client_id = "crab-browser"
public_url = "https://git.example.com"
client_secret_file = "/var/lib/crab/tmp/oidc-client-secret"
state_key_file = "/var/lib/crab/tmp/state-key"
```

Grant the [ECS task role](https://docs.aws.amazon.com/AmazonECS/latest/developerguide/task-iam-roles.html)
bucket-list permission constrained to `repositories` and
object read/write/delete permission constrained to `repositories/*`. The
execution role needs ECR pull, CloudWatch log write, and read access to the
three named Secrets Manager values. Do not place AWS access keys in those
secrets; the server uses the Fargate task role credential chain.

Add an S3 lifecycle rule that expires objects below
`repositories/.crab/http-server/v1/auth/` after 24 hours. Do not apply that
rule to the catalog or repository prefixes.

After replacing every placeholder and digest-pinning a qualified image:

```sh
aws ecs register-task-definition \
  --cli-input-json file://crates/crab-http-server/deploy/ecs/task-definition.example.json

aws ecs create-service \
  --cluster crab \
  --service-name crab-http-server \
  --task-definition crab-http-server \
  --desired-count 2 \
  --launch-type FARGATE \
  --deployment-configuration minimumHealthyPercent=100,maximumPercent=200 \
  --network-configuration 'awsvpcConfiguration={subnets=[subnet-a,subnet-b],securityGroups=[sg-task],assignPublicIp=DISABLED}' \
  --load-balancers targetGroupArn=arn:aws:elasticloadbalancing:REGION:ACCOUNT:targetgroup/NAME/ID,containerName=crab-http-server,containerPort=8788
```

Configure the target group health check to use port `8789` and path `/readyz`;
never expose that port on the ALB listener. Enable the ECS deployment circuit
breaker and span tasks across availability zones. ALB idle timeouts, upstream
request-body limits, and deregistration delay must accommodate the server's
large Git/LFS streams and graceful shutdown budget.

This checked-in task definition is static deployment evidence, not live AWS
qualification. A release claim still requires a real push/fetch/LFS test,
forced task replacement, rolling upgrade, session callback across replicas,
Git-token use across replicas, and safe teardown against a dedicated bucket
prefix.
