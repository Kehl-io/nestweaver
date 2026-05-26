<?php

namespace App\Models;

use App\Contracts\Greeter;
use App\Services\Logger;

interface GreeterInterface
{
    public function greet(string $name): string;
}

trait Loggable
{
    public function log(string $message): void
    {
        echo $message;
    }
}

class SimpleGreeter implements GreeterInterface
{
    use Loggable;

    public function greet(string $name): string
    {
        $this->log("Greeting $name");
        return "Hello, $name!";
    }

    private function formatName(string $name): string
    {
        return ucfirst($name);
    }
}

enum Priority
{
    case Low;
    case Medium;
    case High;
}

function standalone(string $input): string
{
    $greeter = new SimpleGreeter();
    return $greeter->greet($input);
}
