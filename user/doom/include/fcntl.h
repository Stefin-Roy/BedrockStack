/*
 * Minimal freestanding <fcntl.h> for the vendored doomgeneric engine.
 * O_RDONLY only; no engine code actually opens files with flags.
 */
#ifndef BEDROCK_FCNTL_H
#define BEDROCK_FCNTL_H

#define O_RDONLY 0
#define O_WRONLY 1
#define O_RDWR   2
#define O_CREAT  0x40
#define O_TRUNC  0x200
#define O_APPEND 0x400

#endif /* BEDROCK_FCNTL_H */
