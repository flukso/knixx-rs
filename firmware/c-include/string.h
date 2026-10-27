/* The string functions the littlefs C library uses, for the bare-metal
   firmware build, which has no C library: compiler-builtins provides the
   mem* functions, knixx-storage the str* ones. */
#ifndef KNIXX_STRING_H
#define KNIXX_STRING_H

#include <stddef.h>

void *memcpy(void *dst, const void *src, size_t n);
void *memmove(void *dst, const void *src, size_t n);
void *memset(void *s, int c, size_t n);
int memcmp(const void *a, const void *b, size_t n);
size_t strlen(const char *s);
size_t strspn(const char *s, const char *accept);
size_t strcspn(const char *s, const char *reject);
char *strchr(const char *s, int c);
char *strcpy(char *dst, const char *src);

#endif
