#!/bin/sh
# Save password from stdin to /tmp securely for user lsgalante
logger -t save-login-password "Script started for user $PAM_USER"
if [ "$PAM_USER" = "lsgalante" ]; then
    PASSWORD_FILE="/tmp/login-password-lsgalante"
    logger -t save-login-password "Reading password..."
    read -r PASSWORD
    if [ -n "$PASSWORD" ]; then
        logger -t save-login-password "Writing password to $PASSWORD_FILE"
        echo -n "$PASSWORD" > "$PASSWORD_FILE"
        chown lsgalante:lsgalante "$PASSWORD_FILE"
        chmod 600 "$PASSWORD_FILE"
        logger -t save-login-password "Password written successfully"
    else
        logger -t save-login-password "Password was empty!"
    fi
else
    logger -t save-login-password "User was not lsgalante"
fi
