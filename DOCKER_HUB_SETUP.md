# Docker Hub Setup Guide

This guide explains how to set up Docker Hub automated builds for the poe2openai project.

## 1. Docker Hub Setup

### Step 1: Create Docker Hub Repository
1. Go to [Docker Hub](https://hub.docker.com) and log in (create account if needed)
2. Click on "Create Repository" or go to your profile
3. Create a new repository with these details:
   - **Name**: `poe2openai`
   - **Description**: `Poe API to OpenAI API proxy service`
   - **Visibility**: Public (recommended for free tier)
   - **Repository Type**: Standard

### Step 2: Generate Personal Access Token
1. Go to Docker Hub Settings > Security > Personal Access Tokens
2. Click "Generate New Token"
3. Set a descriptive name (e.g., "GitHub Actions CI")
4. Select permissions:
   - ✅ Read
   - ✅ Write
   - ✅ Delete
5. Click "Generate"
6. **IMPORTANT**: Copy the token immediately and store it securely (you won't see it again)

## 2. GitHub Repository Setup

### Step 1: Add Repository Secrets
1. Go to your GitHub repository: `https://github.com/mehmetbaykar/poe2openai`
2. Navigate to Settings > Secrets and variables > Actions
3. Click "New repository secret"
4. Add these secrets:

**Secret 1:**
- Name: `DOCKERHUB_USERNAME`
- Value: `mehmetbaykar`

**Secret 2:**
- Name: `DOCKERHUB_TOKEN`
- Value: `[your-docker-hub-token-from-step-2]`

### Step 2: Push the Changes
1. Commit and push all the files to the main branch:
   ```bash
   git add .
   git commit -m "Add Docker Hub automated publishing setup"
   git push origin main
   ```

## 3. Verify Setup

### Check GitHub Actions
1. Go to your GitHub repository
2. Click on "Actions" tab
3. You should see a workflow run for "Build and Push Docker Image"
4. Wait for it to complete (usually 2-5 minutes)

### Check Docker Hub
1. Go to your Docker Hub repository: `https://hub.docker.com/r/mehmetbaykar/poe2openai`
2. You should see the `latest` tag with the new image
3. The image should show build details and be available for pulling

## 4. Test the Image

### Using Docker CLI
```bash
docker pull mehmetbaykar/poe2openai:latest
docker run -d -p 8080:8080 mehmetbaykar/poe2openai:latest
```

### Using Docker Compose
```bash
docker-compose up -d
```

## 5. Future Updates

Every time you push changes to the main branch, GitHub Actions will:
1. Build the Docker image automatically
2. Push it to Docker Hub as `mehmetbaykar/poe2openai:latest`
3. Users can pull the latest version with: `docker pull mehmetbaykar/poe2openai:latest`

## Troubleshooting

### Common Issues:
1. **Workflow fails**: Check if secrets are correctly set
2. **Image not appearing on Docker Hub**: Verify repository name and token permissions
3. **Build fails**: Check if Dockerfile is valid and all dependencies are available

### Free Tier Limits:
- Docker Hub free tier allows unlimited public repositories
- Build minutes are limited, but for this project it should be sufficient
- If you exceed limits, consider upgrading or optimizing the build process

## Security Notes

- The Docker Hub token has been granted minimal necessary permissions
- Repository is public, so no sensitive data should be included
- Consider rotating the token periodically for security
