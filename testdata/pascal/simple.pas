unit Greeter;

interface

uses SysUtils, Classes;

type
  TAnimal = class
  private
    FName: string;
    FSound: string;
  public
    constructor Create(AName, ASound: string);
    function Speak: string;
    property Name: string read FName;
  end;

  TDog = class(TAnimal)
  public
    constructor Create(AName: string);
  end;

procedure PrintGreeting(const Name: string);
function FormatName(const First, Last: string): string;

implementation

constructor TAnimal.Create(AName, ASound: string);
begin
  FName := AName;
  FSound := ASound;
end;

function TAnimal.Speak: string;
begin
  Result := FName + ' says ' + FSound;
end;

constructor TDog.Create(AName: string);
begin
  inherited Create(AName, 'Woof');
end;

procedure PrintGreeting(const Name: string);
begin
  WriteLn('Hello, ', Name, '!');
end;

function FormatName(const First, Last: string): string;
begin
  Result := First + ' ' + Last;
end;

end.
